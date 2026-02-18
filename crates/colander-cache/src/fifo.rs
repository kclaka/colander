use crate::arena::{Arena, Node};
use crate::traits::{CachePolicy, CacheStats, CachedResponse};
use std::collections::HashMap;
use std::sync::Arc;

/// FIFO (First-In, First-Out) cache eviction policy.
///
/// Baseline comparison. New entries go to head, evictions always from tail.
/// No promotion on access — the simplest possible policy.
pub struct FifoCache {
    arena: Arena,
    map: HashMap<String, u32>,
    capacity: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl FifoCache {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "cache capacity must be > 0");
        Self {
            arena: Arena::new(capacity),
            map: HashMap::with_capacity(capacity),
            capacity,
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }
}

impl CachePolicy for FifoCache {
    fn get(&mut self, key: &str) -> Option<Arc<CachedResponse>> {
        if let Some(&index) = self.map.get(key) {
            let node = self.arena.get(index).unwrap();
            // Check TTL
            if node.value.is_expired() {
                self.misses += 1;
                // Lazy expiration: remove expired entry
                self.map.remove(key);
                self.arena.remove(index);
                return None;
            }
            self.hits += 1;
            // FIFO: no promotion on hit, just return the value
            Some(Arc::clone(&node.value))
        } else {
            self.misses += 1;
            None
        }
    }

    fn insert(&mut self, key: String, value: CachedResponse) {
        // If key already exists, remove old entry first
        if let Some(&old_index) = self.map.get(&key) {
            self.arena.remove(old_index);
            self.map.remove(&key);
        }

        // Evict from tail if at capacity
        while self.arena.len() >= self.capacity {
            if let Some((_, evicted)) = self.arena.pop_tail() {
                self.map.remove(&evicted.key);
                self.evictions += 1;
            } else {
                break;
            }
        }

        let node = Node::new(key.clone(), value);
        if let Some(index) = self.arena.push_head(node) {
            self.map.insert(key, index);
        }
    }

    fn remove(&mut self, key: &str) -> bool {
        if let Some(index) = self.map.remove(key) {
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
        "FIFO"
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
        let mut cache = FifoCache::new(3);
        cache.insert("a".into(), resp(60));
        cache.insert("b".into(), resp(60));
        cache.insert("c".into(), resp(60));

        assert!(cache.get("a").is_some());
        assert!(cache.get("b").is_some());
        assert!(cache.get("c").is_some());
        assert!(cache.get("d").is_none());
    }

    #[test]
    fn evicts_oldest_on_overflow() {
        let mut cache = FifoCache::new(2);
        cache.insert("a".into(), resp(60));
        cache.insert("b".into(), resp(60));
        cache.insert("c".into(), resp(60)); // should evict "a"

        assert!(cache.get("a").is_none()); // evicted
        assert!(cache.get("b").is_some());
        assert!(cache.get("c").is_some());
    }

    #[test]
    fn no_promotion_on_hit() {
        let mut cache = FifoCache::new(2);
        cache.insert("a".into(), resp(60));
        cache.insert("b".into(), resp(60));

        // Access "a" — should NOT promote it
        cache.get("a");

        // Insert "c" — should still evict "a" (oldest by insertion)
        cache.insert("c".into(), resp(60));
        assert!(cache.get("a").is_none());
    }

    #[test]
    fn explicit_remove() {
        let mut cache = FifoCache::new(3);
        cache.insert("a".into(), resp(60));
        assert!(cache.remove("a"));
        assert!(!cache.remove("a")); // already gone
        assert!(cache.get("a").is_none());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn ttl_expiration() {
        let mut cache = FifoCache::new(3);
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

        // Should miss due to TTL
        assert!(cache.get("expired").is_none());
    }

    #[test]
    fn stats_tracking() {
        let mut cache = FifoCache::new(2);
        cache.insert("a".into(), resp(60));
        cache.get("a"); // hit
        cache.get("b"); // miss
        cache.insert("b".into(), resp(60));
        cache.insert("c".into(), resp(60)); // eviction

        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.evictions, 1);
        assert_eq!(stats.current_size, 2);
        assert_eq!(stats.capacity, 2);
    }

    #[test]
    fn reinsert_same_key() {
        let mut cache = FifoCache::new(2);
        cache.insert("a".into(), resp(60));
        cache.insert("b".into(), resp(60));
        cache.insert("a".into(), resp(60)); // re-insert should update, not double-count

        assert_eq!(cache.len(), 2);
        assert!(cache.get("a").is_some());
        assert!(cache.get("b").is_some());
    }
}
