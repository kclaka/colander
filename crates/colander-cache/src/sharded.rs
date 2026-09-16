use crate::traits::{CachePolicy, CacheStats, CachedResponse};
use parking_lot::RwLock;
use std::sync::{Arc, OnceLock};

/// Maximum number of independent shards.
const NUM_SHARDS: usize = 64;

/// Thread-safe sharded cache wrapper. All policy lookups currently acquire a
/// write lock because the common policy API updates statistics and expires entries.
/// A process-wide random hash seed keeps policy comparisons on the same shards
/// without exposing a fixed, predictable mapping to clients.
pub struct ShardedCache<T: CachePolicy> {
    shards: Box<[RwLock<T>]>,
    name: &'static str,
}

impl<T: CachePolicy> ShardedCache<T> {
    /// Create up to 64 shards, distributing every requested slot exactly once.
    /// Panics for zero capacity, like the underlying cache policies.
    pub fn new<F>(total_capacity: usize, make_shard: F) -> Self
    where
        F: Fn(usize) -> T,
    {
        assert!(total_capacity > 0, "cache capacity must be > 0");
        let count = total_capacity.min(NUM_SHARDS);
        let shards: Box<[RwLock<T>]> = (0..count)
            .map(|index| {
                let capacity = total_capacity / count + usize::from(index < total_capacity % count);
                RwLock::new(make_shard(capacity))
            })
            .collect();
        let name = shards[0].read().name();
        Self { shards, name }
    }

    #[inline]
    fn shard_index(&self, key: &str) -> usize {
        static HASHER: OnceLock<ahash::RandomState> = OnceLock::new();
        let hash = HASHER.get_or_init(ahash::RandomState::new).hash_one(key);
        (hash % self.shards.len() as u64) as usize
    }

    /// Look up a key under the policy's exclusive shard lock.
    pub fn get(&self, key: &str) -> Option<Arc<CachedResponse>> {
        let idx = self.shard_index(key);
        self.shards[idx].write().get(key)
    }

    /// Insert a key-value pair. Takes a write lock on one shard.
    pub fn insert(&self, key: String, value: CachedResponse) {
        let idx = self.shard_index(&key);
        let mut shard = self.shards[idx].write();
        shard.insert(key, value);
    }

    /// Remove a key explicitly.
    pub fn remove(&self, key: &str) -> bool {
        let idx = self.shard_index(key);
        let mut shard = self.shards[idx].write();
        shard.remove(key)
    }

    /// Total number of entries across all shards.
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.read().len()).sum()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|s| s.read().is_empty())
    }

    /// Total capacity across all shards.
    pub fn capacity(&self) -> usize {
        self.shards.iter().map(|s| s.read().capacity()).sum()
    }

    /// Name of the underlying eviction policy.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Aggregate statistics across all shards.
    pub fn stats(&self) -> CacheStats {
        let mut total = CacheStats::default();
        for shard in self.shards.iter() {
            let s = shard.read().stats();
            total.hits += s.hits;
            total.misses += s.misses;
            total.evictions += s.evictions;
            total.current_size += s.current_size;
            total.capacity += s.capacity;
        }
        total
    }
}

// Send and Sync are derived from the policy and lock types.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fifo::FifoCache;
    use crate::lru::LruCache;
    use crate::sieve::SieveCache;
    use bytes::Bytes;
    use std::time::{Duration, Instant};

    fn resp() -> CachedResponse {
        CachedResponse {
            status: 200,
            headers: vec![],
            body: Bytes::from_static(b"test"),
            inserted_at: Instant::now(),
            ttl: Duration::from_secs(60),
        }
    }

    #[test]
    fn preserves_exact_capacity_for_every_policy() {
        fn check<T: CachePolicy>(factory: fn(usize) -> T) {
            for capacity in [1, 2, 63, 64, 65, 127, 10000] {
                let cache = ShardedCache::new(capacity, factory);
                assert_eq!(cache.capacity(), capacity);
                assert_eq!(cache.stats().capacity, capacity);
                for i in 0..capacity * 2 {
                    cache.insert(format!("key-{i}"), resp());
                }
                assert!(cache.len() <= capacity);
            }
        }
        check(SieveCache::new);
        check(LruCache::new);
        check(FifoCache::new);
    }

    #[test]
    #[should_panic(expected = "cache capacity must be > 0")]
    fn rejects_zero_capacity() {
        ShardedCache::new(0, SieveCache::new);
    }

    #[test]
    fn policies_share_the_same_shard_mapping() {
        let sieve = ShardedCache::new(65, SieveCache::new);
        let lru = ShardedCache::new(65, LruCache::new);
        for i in 0..1000 {
            let key = format!("key-{i}");
            assert_eq!(sieve.shard_index(&key), lru.shard_index(&key));
        }
    }

    #[test]
    fn sharded_sieve_basic() {
        let cache = ShardedCache::new(1024, SieveCache::new);

        cache.insert("hello".into(), resp());
        assert!(cache.get("hello").is_some());
        assert!(cache.get("missing").is_none());
        assert_eq!(cache.name(), "SIEVE");
    }

    #[test]
    fn sharded_lru_basic() {
        let cache = ShardedCache::new(1024, LruCache::new);

        cache.insert("hello".into(), resp());
        assert!(cache.get("hello").is_some());
        assert!(cache.get("missing").is_none());
        assert_eq!(cache.name(), "LRU");
    }

    #[test]
    fn sharded_fifo_basic() {
        let cache = ShardedCache::new(1024, FifoCache::new);

        cache.insert("hello".into(), resp());
        assert!(cache.get("hello").is_some());
        assert_eq!(cache.name(), "FIFO");
    }

    #[test]
    fn distributes_across_shards() {
        let cache = ShardedCache::new(4096, SieveCache::new);

        // Insert enough keys that they should spread across multiple shards
        for i in 0..200 {
            cache.insert(format!("key-{}", i), resp());
        }

        assert_eq!(cache.len(), 200);

        // Verify at least some shards have entries (not all in one shard)
        let nonempty_shards = cache.shards.iter().filter(|s| s.read().len() > 0).count();
        assert!(
            nonempty_shards > 1,
            "expected keys distributed across multiple shards, got {}",
            nonempty_shards
        );
    }

    #[test]
    fn remove_works() {
        let cache = ShardedCache::new(1024, SieveCache::new);

        cache.insert("a".into(), resp());
        assert!(cache.get("a").is_some());
        assert!(cache.remove("a"));
        assert!(cache.get("a").is_none());
        assert!(!cache.remove("a")); // already gone
    }

    #[test]
    fn stats_aggregate() {
        let cache = ShardedCache::new(1024, SieveCache::new);

        cache.insert("a".into(), resp());
        cache.insert("b".into(), resp());
        cache.get("a"); // hit
        cache.get("z"); // miss

        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.current_size, 2);
    }

    #[test]
    fn eviction_within_shard() {
        // Small total capacity — each shard gets very few slots
        let cache = ShardedCache::new(64, SieveCache::new);

        // Insert many more keys than capacity
        for i in 0..500 {
            cache.insert(format!("key-{}", i), resp());
        }

        // Total size should not exceed capacity
        assert!(
            cache.len() <= cache.capacity(),
            "len {} exceeded capacity {}",
            cache.len(),
            cache.capacity()
        );

        let stats = cache.stats();
        assert!(stats.evictions > 0, "expected evictions to occur");
    }

    #[test]
    fn ttl_expiration_through_sharded() {
        let cache = ShardedCache::new(1024, SieveCache::new);

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
    fn concurrent_access() {
        use std::sync::Arc;
        use std::thread;

        let cache = Arc::new(ShardedCache::new(4096, SieveCache::new));

        // Pre-populate
        for i in 0..1000 {
            cache.insert(format!("key-{}", i), resp());
        }

        // Spawn readers and writers concurrently
        let mut handles = vec![];

        for t in 0..8 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for i in 0..1000 {
                    let key = format!("key-{}", (t * 1000 + i) % 2000);
                    if i % 3 == 0 {
                        cache.insert(key, resp());
                    } else {
                        cache.get(&key);
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Should not panic or deadlock. Cache should be consistent.
        assert!(cache.len() <= cache.capacity());
        let stats = cache.stats();
        assert!(stats.hits + stats.misses > 0);
    }

    #[test]
    fn is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ShardedCache<SieveCache>>();
        assert_send_sync::<ShardedCache<LruCache>>();
        assert_send_sync::<ShardedCache<FifoCache>>();
    }
}
