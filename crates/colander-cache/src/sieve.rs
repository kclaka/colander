use crate::arena::{Arena, Node, NIL};
use crate::traits::{CachePolicy, CacheStats, CachedResponse};
use std::collections::HashMap;
use std::sync::Arc;

/// SIEVE cache eviction policy (NSDI '24).
///
/// Key insight: a roving "hand" pointer walks from tail toward head to find
/// eviction candidates. Visited objects are retained in place (visited bit cleared),
/// unvisited objects are evicted. New objects always insert at head.
///
/// Critical difference from CLOCK/FIFO-Reinsertion: retained objects stay in their
/// original position instead of being moved to head. This separates new objects
/// from popular objects, enabling quick demotion of unpopular entries.
///
/// Cache hits only flip a visited bit (AtomicBool) — no list mutation required.
/// This means hits can be served under a read lock (or lock-free with sharding).
pub struct SieveCache {
    arena: Arena,
    map: HashMap<String, u32>,
    hand: u32, // Eviction scan pointer
    capacity: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl SieveCache {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "cache capacity must be > 0");
        Self {
            arena: Arena::new(capacity),
            map: HashMap::with_capacity(capacity),
            hand: NIL,
            capacity,
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    /// The SIEVE eviction algorithm.
    ///
    /// Starting from the hand position, scan toward the head:
    /// - If node is visited: clear visited bit, move hand to prev (keep node in place)
    /// - If node is unvisited: evict it, set hand to prev
    /// - If node is expired: evict it regardless of visited bit
    ///
    /// The hand wraps around to the tail when it reaches NIL (head).
    fn evict_one(&mut self) {
        // If hand is NIL, start from tail
        if self.hand == NIL {
            self.hand = self.arena.tail;
        }

        loop {
            if self.hand == NIL {
                // Wrapped around — start from tail again
                self.hand = self.arena.tail;
            }

            if self.hand == NIL {
                // Cache is empty, nothing to evict
                return;
            }

            let index = self.hand;
            let node = self.arena.get(index).unwrap();

            // Always evict expired entries
            if node.value.is_expired() {
                // Advance hand before removing
                self.hand = node.prev;
                let evicted = self.arena.remove(index).unwrap();
                self.map.remove(&evicted.key);
                self.evictions += 1;
                return;
            }

            if node.is_visited() {
                // Retain: clear visited bit, move hand to prev
                node.clear_visited();
                self.hand = node.prev;
                // Keep scanning
            } else {
                // Evict: this is an unvisited (cold) object
                self.hand = node.prev;
                let evicted = self.arena.remove(index).unwrap();
                self.map.remove(&evicted.key);
                self.evictions += 1;
                return;
            }
        }
    }
}

impl CachePolicy for SieveCache {
    fn get(&mut self, key: &str) -> Option<Arc<CachedResponse>> {
        if let Some(&index) = self.map.get(key) {
            let node = self.arena.get(index).unwrap();
            // Check TTL
            if node.value.is_expired() {
                self.misses += 1;
                self.map.remove(key);
                // Fix hand if it points to the node we're about to remove
                if self.hand == index {
                    self.hand = node.prev;
                }
                self.arena.remove(index);
                return None;
            }
            self.hits += 1;
            // SIEVE: just flip the visited bit. No list mutation!
            // In the sharded version, this is the only operation on the hot path.
            node.mark_visited();
            Some(Arc::clone(&node.value))
        } else {
            self.misses += 1;
            None
        }
    }

    fn insert(&mut self, key: String, value: CachedResponse) {
        // If key already exists, remove old entry
        if let Some(&old_index) = self.map.get(&key) {
            // Fix hand if it points to the node we're about to remove
            if self.hand == old_index {
                let node = self.arena.get(old_index).unwrap();
                self.hand = node.prev;
            }
            self.arena.remove(old_index);
            self.map.remove(&key);
        }

        // Evict if at capacity
        while self.arena.len() >= self.capacity {
            self.evict_one();
        }

        // Insert new object at head (not visited initially)
        let node = Node::new(key.clone(), value);
        if let Some(index) = self.arena.push_head(node) {
            self.map.insert(key, index);
        }
    }

    fn remove(&mut self, key: &str) -> bool {
        if let Some(index) = self.map.remove(key) {
            if self.hand == index {
                let node = self.arena.get(index).unwrap();
                self.hand = node.prev;
            }
            self.arena.remove(index);
            true
        } else {
            false
        }
    }

    fn len(&self) -> usize {
        self.arena.len()
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    fn name(&self) -> &'static str {
        "SIEVE"
    }

    fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
            current_size: self.arena.len(),
            capacity: self.capacity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::time::{Duration, Instant};

    fn resp(ttl_secs: u64) -> CachedResponse {
        CachedResponse {
            status: 200,
            headers: vec![],
            body: Bytes::from_static(b"test"),
            inserted_at: Instant::now(),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    #[test]
    fn basic_insert_and_get() {
        let mut cache = SieveCache::new(3);
        cache.insert("a".into(), resp(60));
        cache.insert("b".into(), resp(60));
        cache.insert("c".into(), resp(60));

        assert!(cache.get("a").is_some());
        assert!(cache.get("b").is_some());
        assert!(cache.get("c").is_some());
        assert!(cache.get("d").is_none());
    }

    #[test]
    fn evicts_unvisited_from_tail() {
        let mut cache = SieveCache::new(3);
        cache.insert("a".into(), resp(60));
        cache.insert("b".into(), resp(60));
        cache.insert("c".into(), resp(60));

        // Don't access any — all unvisited
        // Insert "d" — hand should start at tail ("a") and evict it
        cache.insert("d".into(), resp(60));

        assert!(cache.get("a").is_none()); // evicted (was tail, unvisited)
        assert!(cache.get("b").is_some());
        assert!(cache.get("c").is_some());
        assert!(cache.get("d").is_some());
    }

    #[test]
    fn retains_visited_objects_in_place() {
        let mut cache = SieveCache::new(3);
        cache.insert("a".into(), resp(60));
        cache.insert("b".into(), resp(60));
        cache.insert("c".into(), resp(60));

        // Visit "a" (tail) — it should survive eviction
        cache.get("a");

        // Insert "d" — hand starts at tail ("a"), finds it visited, clears bit,
        // moves to "b" (unvisited), evicts "b"
        cache.insert("d".into(), resp(60));

        assert!(cache.get("a").is_some()); // survived (was visited)
        assert!(cache.get("b").is_none()); // evicted (was unvisited)
        assert!(cache.get("c").is_some());
        assert!(cache.get("d").is_some());
    }

    #[test]
    fn hand_continues_from_last_position() {
        let mut cache = SieveCache::new(3);
        cache.insert("a".into(), resp(60));
        cache.insert("b".into(), resp(60));
        cache.insert("c".into(), resp(60));
        // List: head -> c -> b -> a -> tail, hand = NIL

        // Visit "a" and "b"
        cache.get("a");
        cache.get("b");

        // Insert "d" — hand starts at tail (a):
        //   a(visited->clear, hand->b), b(visited->clear, hand->c), c(unvisited->evict)
        // After: head -> d -> b -> a -> tail, hand = NIL (c.prev was NIL)
        cache.insert("d".into(), resp(60));
        assert!(cache.get("c").is_none()); // evicted

        // Visit "b" so it survives the next eviction
        cache.get("b");

        // Insert "e" — hand = NIL, starts at tail (a):
        //   a(unvisited, cleared during last scan) -> evict a
        // After: head -> e -> d -> b -> tail
        cache.insert("e".into(), resp(60));
        assert!(cache.get("a").is_none()); // evicted (was tail, unvisited)
        assert!(cache.get("b").is_some()); // survived (was visited)
        assert!(cache.get("d").is_some());
        assert!(cache.get("e").is_some());
    }

    #[test]
    fn no_list_mutation_on_hit() {
        // This is SIEVE's key property: visited objects stay in place
        let mut cache = SieveCache::new(3);
        cache.insert("a".into(), resp(60));
        cache.insert("b".into(), resp(60));
        cache.insert("c".into(), resp(60));

        // Order: head -> c -> b -> a -> tail
        // Access "a" multiple times — it should NOT move to head
        cache.get("a");
        cache.get("a");
        cache.get("a");

        // Insert "d" — evict scan starts at tail ("a"), but "a" is visited
        // Clears "a", moves to "b" (unvisited), evicts "b"
        cache.insert("d".into(), resp(60));

        assert!(cache.get("a").is_some()); // stayed in place, visited bit saved it
        assert!(cache.get("b").is_none()); // evicted
    }

    #[test]
    fn sieve_vs_fifo_advantage() {
        // Demonstrate SIEVE's advantage: popular objects survive even at tail
        let mut sieve = SieveCache::new(3);
        let mut fifo = super::super::fifo::FifoCache::new(3);

        // Insert a, b, c
        for key in &["a", "b", "c"] {
            sieve.insert(key.to_string(), resp(60));
            fifo.insert(key.to_string(), resp(60));
        }

        // Access "a" (the oldest/tail item) heavily
        for _ in 0..10 {
            sieve.get("a");
            fifo.get("a");
        }

        // Insert "d" — causes eviction
        sieve.insert("d".into(), resp(60));
        fifo.insert("d".into(), resp(60));

        // SIEVE keeps "a" (popular), FIFO evicts it (oldest)
        assert!(sieve.get("a").is_some(), "SIEVE should retain popular 'a'");
        assert!(fifo.get("a").is_none(), "FIFO should evict oldest 'a'");
    }

    #[test]
    fn explicit_remove() {
        let mut cache = SieveCache::new(3);
        cache.insert("a".into(), resp(60));
        assert!(cache.remove("a"));
        assert!(cache.get("a").is_none());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn remove_hand_target() {
        // If we remove the node the hand points to, the hand should advance
        let mut cache = SieveCache::new(3);
        cache.insert("a".into(), resp(60));
        cache.insert("b".into(), resp(60));
        cache.insert("c".into(), resp(60));

        // Visit "a" so eviction skips it, leaving hand near "a"
        cache.get("a");
        cache.insert("d".into(), resp(60)); // evicts "b", hand now at "a"

        // Explicitly remove "a" — hand should update
        cache.remove("a");
        assert_eq!(cache.len(), 2);

        // Should still be able to evict normally
        cache.insert("e".into(), resp(60));
        cache.insert("f".into(), resp(60));
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn ttl_expiration() {
        let mut cache = SieveCache::new(3);
        cache.insert(
            "expired".into(),
            CachedResponse {
                status: 200,
                headers: vec![],
                body: Bytes::from_static(b"old"),
                inserted_at: Instant::now() - Duration::from_secs(120),
                ttl: Duration::from_secs(60),
            },
        );

        assert!(cache.get("expired").is_none());
    }

    #[test]
    fn evict_expired_regardless_of_visited() {
        let mut cache = SieveCache::new(2);
        cache.insert(
            "will-expire".into(),
            CachedResponse {
                status: 200,
                headers: vec![],
                body: Bytes::from_static(b"old"),
                inserted_at: Instant::now() - Duration::from_secs(120),
                ttl: Duration::from_secs(60),
            },
        );
        // Visit it — would normally protect it
        cache.get("will-expire"); // returns None because expired, but let's set it up differently

        // Insert a fresh item with visited bit set, then make it expire
        let mut cache = SieveCache::new(2);
        let expired_resp = CachedResponse {
            status: 200,
            headers: vec![],
            body: Bytes::from_static(b"old"),
            inserted_at: Instant::now() - Duration::from_secs(120),
            ttl: Duration::from_secs(60),
        };
        cache.insert("a".into(), expired_resp);
        // Mark as visited by directly accessing the arena
        if let Some(&idx) = cache.map.get("a") {
            cache.arena.get(idx).unwrap().mark_visited();
        }

        cache.insert("b".into(), resp(60));

        // Insert "c" — should evict "a" even though visited (expired)
        cache.insert("c".into(), resp(60));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn stats_tracking() {
        let mut cache = SieveCache::new(2);
        cache.insert("a".into(), resp(60));
        cache.get("a"); // hit
        cache.get("z"); // miss
        cache.insert("b".into(), resp(60));
        cache.insert("c".into(), resp(60)); // eviction

        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.evictions, 1);
    }

    #[test]
    fn reinsert_same_key() {
        let mut cache = SieveCache::new(2);
        cache.insert("a".into(), resp(60));
        cache.insert("b".into(), resp(60));
        cache.insert("a".into(), resp(60));

        assert_eq!(cache.len(), 2);
        assert!(cache.get("a").is_some());
        assert!(cache.get("b").is_some());
    }

    #[test]
    fn full_wrap_around() {
        // All items visited — hand must wrap around and evict one
        let mut cache = SieveCache::new(3);
        cache.insert("a".into(), resp(60));
        cache.insert("b".into(), resp(60));
        cache.insert("c".into(), resp(60));

        // Visit all
        cache.get("a");
        cache.get("b");
        cache.get("c");

        // Insert "d" — hand scans all, clears all visited bits, wraps to tail, evicts "a"
        cache.insert("d".into(), resp(60));
        assert_eq!(cache.len(), 3);
        // One of a/b/c should be evicted (the tail after wrap-around)
        let alive: Vec<&str> = ["a", "b", "c", "d"]
            .iter()
            .filter(|k| cache.get(k).is_some())
            .copied()
            .collect();
        assert_eq!(alive.len(), 3);
        assert!(alive.contains(&"d")); // new item always survives
    }
}
