use bytes::Bytes;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Cached HTTP response stored in the cache.
#[derive(Clone, Debug)]
pub struct CachedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    pub inserted_at: Instant,
    pub ttl: Duration,
}

impl CachedResponse {
    pub fn is_expired(&self) -> bool {
        self.inserted_at.elapsed() > self.ttl
    }
}

/// Snapshot of cache statistics.
#[derive(Clone, Debug, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub current_size: usize,
    pub capacity: usize,
}

/// Common interface for all cache eviction policies.
///
/// Implementations: SIEVE, LRU, FIFO.
/// All methods take `&mut self` — thread safety is handled by the sharded wrapper.
pub trait CachePolicy: Send {
    /// Look up a key. Returns the cached response if found and not expired.
    fn get(&mut self, key: &str) -> Option<Arc<CachedResponse>>;

    /// Insert a key-value pair. May trigger eviction if at capacity.
    fn insert(&mut self, key: String, value: CachedResponse);

    /// Remove a key explicitly.
    fn remove(&mut self, key: &str) -> bool;

    /// Number of entries currently in the cache.
    fn len(&self) -> usize;

    /// Whether the cache is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Maximum number of entries.
    fn capacity(&self) -> usize;

    /// Human-readable name of the eviction policy.
    fn name(&self) -> &'static str;

    /// Current statistics snapshot.
    fn stats(&self) -> CacheStats;
}
