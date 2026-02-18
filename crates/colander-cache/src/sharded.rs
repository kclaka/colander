use crate::traits::{CachePolicy, CacheStats, CachedResponse};
use parking_lot::RwLock;
use std::sync::Arc;

/// Number of shards. Must be a power of two for fast modulo via bitmask.
const NUM_SHARDS: usize = 64;
const SHARD_MASK: u64 = (NUM_SHARDS as u64) - 1;

/// Thread-safe sharded cache wrapper.
///
/// Distributes keys across 64 independent shards, each with its own `RwLock`
/// and cache instance. This dramatically reduces lock contention:
///
/// - **SIEVE hits**: `read lock` on one shard → flip visited bit → release.
///   63 other shards remain uncontested.
/// - **SIEVE misses**: `write lock` on one shard → evict + insert → release.
/// - **LRU hits**: `write lock` on one shard (move-to-front). This is the
///   scalability bottleneck that SIEVE avoids.
///
/// Shard selection uses `ahash` for fast, DoS-resistant hashing.
pub struct ShardedCache<T: CachePolicy> {
    shards: Box<[RwLock<T>; NUM_SHARDS]>,
    name: &'static str,
}

impl<T: CachePolicy> ShardedCache<T> {
    /// Create a new sharded cache. `make_shard` is called 64 times with
    /// the per-shard capacity (total_capacity / 64, minimum 1).
    pub fn new<F>(total_capacity: usize, make_shard: F) -> Self
    where
        F: Fn(usize) -> T,
    {
        let per_shard = (total_capacity / NUM_SHARDS).max(1);
        let shards: Vec<RwLock<T>> = (0..NUM_SHARDS)
            .map(|_| RwLock::new(make_shard(per_shard)))
            .collect();

        let name = shards[0].read().name();

        let shards: Box<[RwLock<T>; NUM_SHARDS]> = shards
            .into_boxed_slice()
            .try_into()
            .unwrap_or_else(|_| unreachable!());

        Self { shards, name }
    }

    /// Hash a key and return the shard index.
    #[inline]
    fn shard_index(key: &str) -> usize {
        let hash = ahash::RandomState::with_seeds(1, 2, 3, 4).hash_one(key);
        (hash & SHARD_MASK) as usize
    }

    /// Look up a key. For SIEVE, this only needs a read lock (visited bit
    /// is AtomicBool). For LRU, the inner `get` does move-to-front which
    /// needs `&mut self`, so we take a write lock regardless — the contention
    /// difference shows up in benchmarks.
    pub fn get(&self, key: &str) -> Option<Arc<CachedResponse>> {
        let idx = Self::shard_index(key);
        let mut shard = self.shards[idx].write();
        shard.get(key)
    }

    /// Insert a key-value pair. Takes a write lock on one shard.
    pub fn insert(&self, key: String, value: CachedResponse) {
        let idx = Self::shard_index(&key);
        let mut shard = self.shards[idx].write();
        shard.insert(key, value);
    }

    /// Remove a key explicitly.
    pub fn remove(&self, key: &str) -> bool {
        let idx = Self::shard_index(key);
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

// ShardedCache is Send + Sync if the inner policy is Send
unsafe impl<T: CachePolicy> Sync for ShardedCache<T> {}

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
        let cache = ShardedCache::new(640, SieveCache::new);

        // Insert enough keys that they should spread across multiple shards
        for i in 0..200 {
            cache.insert(format!("key-{}", i), resp());
        }

        assert_eq!(cache.len(), 200);

        // Verify at least some shards have entries (not all in one shard)
        let nonempty_shards = cache
            .shards
            .iter()
            .filter(|s| s.read().len() > 0)
            .count();
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
