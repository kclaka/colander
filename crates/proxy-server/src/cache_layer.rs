use colander_cache::fifo::FifoCache;
use colander_cache::lru::LruCache;
use colander_cache::sharded::ShardedCache;
use colander_cache::sieve::SieveCache;
use colander_cache::traits::{CacheStats, CachedResponse};

use bytes::Bytes;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Runtime mode for the dual-cache system.
/// - Demo: updates both caches, serves from primary (fair hit-rate comparison)
/// - Bench: updates only primary cache (true latency/throughput)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    Demo,
    Bench,
}

/// Type-erased cache that wraps a ShardedCache with any policy.
enum CacheInner {
    Sieve(ShardedCache<SieveCache>),
    Lru(ShardedCache<LruCache>),
    Fifo(ShardedCache<FifoCache>),
}

impl CacheInner {
    fn peek(&self, key: &str) -> Option<Arc<CachedResponse>> {
        match self {
            CacheInner::Sieve(c) => c.peek(key),
            CacheInner::Lru(c) => c.peek(key),
            CacheInner::Fifo(c) => c.peek(key),
        }
    }

    fn get(&self, key: &str) -> Option<Arc<CachedResponse>> {
        match self {
            CacheInner::Sieve(c) => c.get(key),
            CacheInner::Lru(c) => c.get(key),
            CacheInner::Fifo(c) => c.get(key),
        }
    }

    fn insert(&self, key: String, value: CachedResponse) {
        match self {
            CacheInner::Sieve(c) => c.insert(key, value),
            CacheInner::Lru(c) => c.insert(key, value),
            CacheInner::Fifo(c) => c.insert(key, value),
        }
    }

    fn remove(&self, key: &str) -> bool {
        match self {
            CacheInner::Sieve(c) => c.remove(key),
            CacheInner::Lru(c) => c.remove(key),
            CacheInner::Fifo(c) => c.remove(key),
        }
    }

    fn stats(&self) -> CacheStats {
        match self {
            CacheInner::Sieve(c) => c.stats(),
            CacheInner::Lru(c) => c.stats(),
            CacheInner::Fifo(c) => c.stats(),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            CacheInner::Sieve(c) => c.name(),
            CacheInner::Lru(c) => c.name(),
            CacheInner::Fifo(c) => c.name(),
        }
    }
}

fn build_cache(policy: &str, capacity: usize) -> CacheInner {
    match policy {
        "sieve" => CacheInner::Sieve(ShardedCache::new(capacity, SieveCache::new)),
        "lru" => CacheInner::Lru(ShardedCache::new(capacity, LruCache::new)),
        "fifo" => CacheInner::Fifo(ShardedCache::new(capacity, FifoCache::new)),
        other => panic!("unknown eviction policy: {other}"),
    }
}

/// Dual-cache layer for the proxy.
///
/// Primary cache serves responses. Comparison cache (optional) runs in shadow
/// mode for metrics only. Toggle between demo and bench mode at runtime.
pub struct CacheLayer {
    primary: CacheInner,
    comparison: Option<CacheInner>,
    demo_mode: AtomicBool,
    default_ttl_secs: AtomicU64,
    pub max_body_size: usize,
}

impl CacheLayer {
    pub fn new(
        primary_policy: &str,
        comparison_policy: Option<&str>,
        capacity: usize,
        default_ttl: Duration,
        max_body_size: usize,
    ) -> Self {
        let primary = build_cache(primary_policy, capacity);
        let comparison = comparison_policy.map(|p| build_cache(p, capacity));

        tracing::info!(
            primary = primary.name(),
            comparison = comparison.as_ref().map(|c| c.name()),
            capacity,
            "cache layer initialized"
        );

        Self {
            primary,
            comparison,
            demo_mode: AtomicBool::new(true),
            default_ttl_secs: AtomicU64::new(default_ttl.as_secs()),
            max_body_size,
        }
    }

    /// Current default TTL (read atomically for hot-reload support).
    pub fn default_ttl(&self) -> Duration {
        Duration::from_secs(self.default_ttl_secs.load(Ordering::Relaxed))
    }

    /// Update the default TTL atomically (no cache data loss).
    pub fn set_default_ttl(&self, secs: u64) {
        self.default_ttl_secs.store(secs, Ordering::Relaxed);
    }

    /// Look up a key in the primary cache. In demo mode, also checks the
    /// comparison cache (for metrics only — result is discarded).
    pub fn get(&self, key: &str) -> CacheLookup {
        let primary_result = self.primary.get(key);

        let comparison_hit = if self.is_demo_mode() {
            if let Some(comp) = &self.comparison {
                let hit = comp.get(key).is_some();
                // A shadow miss still needs the object when primary serves a hit.
                if !hit {
                    if let Some(value) = &primary_result {
                        comp.insert(key.to_owned(), (**value).clone());
                    }
                }
                hit
            } else {
                false
            }
        } else {
            false
        };

        CacheLookup {
            value: primary_result,
            comparison_hit,
        }
    }

    /// Insert into primary cache. In demo mode, also inserts into comparison.
    pub fn insert(&self, key: String, value: CachedResponse) {
        if self.is_demo_mode() {
            if let Some(comp) = &self.comparison {
                // A primary miss may already be a shadow hit. Preserve its entry
                // and policy state; a second lookup must not skew hit counters.
                if comp.peek(&key).is_none() {
                    comp.insert(key.clone(), value.clone());
                }
            }
        }
        self.primary.insert(key, value);
    }

    /// Remove a key from the primary cache. Returns true if the key existed.
    pub fn remove(&self, key: &str) -> bool {
        self.primary.remove(key)
    }

    /// Insert raw bytes (for RESP SET — bypasses HTTP response wrapping).
    /// Only inserts into primary (RESP ops don't participate in demo comparison).
    pub fn insert_raw(&self, key: String, value: Bytes, ttl: Option<Duration>) {
        let response = CachedResponse {
            status: 0,
            headers: vec![],
            body: value,
            inserted_at: Instant::now(),
            ttl: ttl.unwrap_or(self.default_ttl()),
        };
        self.primary.insert(key, response);
    }

    /// Get TTL remaining for a key. Returns None if key missing/expired.
    pub fn ttl_remaining(&self, key: &str) -> Option<Duration> {
        let entry = self.primary.get(key)?;
        entry.ttl.checked_sub(entry.inserted_at.elapsed())
    }

    /// Build a CachedResponse from raw HTTP response parts.
    pub fn build_response(
        &self,
        status: u16,
        headers: Vec<(String, String)>,
        body: Bytes,
        ttl: Option<Duration>,
    ) -> CachedResponse {
        CachedResponse {
            status,
            headers,
            body,
            inserted_at: Instant::now(),
            ttl: ttl.unwrap_or(self.default_ttl()),
        }
    }

    pub fn primary_stats(&self) -> CacheStats {
        self.primary.stats()
    }

    pub fn comparison_stats(&self) -> Option<CacheStats> {
        self.comparison.as_ref().map(|c| c.stats())
    }

    pub fn primary_name(&self) -> &'static str {
        self.primary.name()
    }

    pub fn comparison_name(&self) -> Option<&'static str> {
        self.comparison.as_ref().map(|c| c.name())
    }

    pub fn is_demo_mode(&self) -> bool {
        self.demo_mode.load(Ordering::Relaxed)
    }

    pub fn set_mode(&self, mode: CacheMode) {
        self.demo_mode
            .store(mode == CacheMode::Demo, Ordering::Relaxed);
        tracing::info!(?mode, "cache mode changed");
    }

    pub fn mode(&self) -> CacheMode {
        if self.is_demo_mode() {
            CacheMode::Demo
        } else {
            CacheMode::Bench
        }
    }
}

/// Result of a cache lookup, including comparison cache info.
pub struct CacheLookup {
    pub value: Option<Arc<CachedResponse>>,
    pub comparison_hit: bool,
}

impl CacheLookup {
    pub fn is_hit(&self) -> bool {
        self.value.is_some()
    }
}

/// Parse Cache-Control header to determine cacheability and TTL.
pub fn parse_cache_control(value: &str) -> CacheControl {
    let mut result = CacheControl {
        cacheable: true,
        max_age: None,
    };

    for directive in value.split(',').map(|s| s.trim().to_lowercase()) {
        if directive == "no-store" || directive == "no-cache" || directive == "private" {
            result.cacheable = false;
        }
        if let Some(age) = directive.strip_prefix("max-age=") {
            if let Ok(secs) = age.trim().parse::<u64>() {
                result.max_age = Some(Duration::from_secs(secs));
            }
        }
        if let Some(age) = directive.strip_prefix("s-maxage=") {
            if let Ok(secs) = age.trim().parse::<u64>() {
                // s-maxage takes precedence for shared caches
                result.max_age = Some(Duration::from_secs(secs));
            }
        }
    }

    result
}

pub struct CacheControl {
    pub cacheable: bool,
    pub max_age: Option<Duration>,
}

#[cfg(test)]
mod comparison_tests {
    use super::*;

    fn cache() -> CacheLayer {
        CacheLayer::new("sieve", Some("lru"), 1024, Duration::from_secs(60), 1024)
    }

    #[test]
    fn primary_hit_populates_a_shadow_miss_without_extra_statistics() {
        let cache = cache();
        cache.insert(
            "key".into(),
            cache.build_response(200, vec![], Bytes::from_static(b"value"), None),
        );
        cache.comparison.as_ref().unwrap().remove("key");
        let lookup = cache.get("key");
        assert!(lookup.is_hit());
        assert!(!lookup.comparison_hit);
        let stats = cache.comparison_stats().unwrap();
        assert_eq!((stats.hits, stats.misses, stats.current_size), (0, 1, 1));
        assert!(cache.get("key").comparison_hit);
    }

    #[test]
    fn primary_miss_does_not_replace_a_shadow_hit_or_count_an_extra_lookup() {
        let cache = cache();
        cache.insert(
            "key".into(),
            cache.build_response(200, vec![], Bytes::from_static(b"old"), None),
        );
        cache.primary.remove("key");
        let lookup = cache.get("key");
        assert!(!lookup.is_hit());
        assert!(lookup.comparison_hit);
        cache.insert(
            "key".into(),
            cache.build_response(200, vec![], Bytes::from_static(b"fresh"), None),
        );
        let comparison = cache.comparison.as_ref().unwrap();
        assert_eq!(comparison.peek("key").unwrap().body, "old");
        assert_eq!((comparison.stats().hits, comparison.stats().misses), (1, 0));
    }

    #[test]
    fn bench_mode_does_not_populate_or_touch_shadow_cache() {
        let cache = cache();
        cache.set_mode(CacheMode::Bench);
        cache.insert(
            "key".into(),
            cache.build_response(200, vec![], Bytes::from_static(b"value"), None),
        );
        cache.get("key");
        let stats = cache.comparison_stats().unwrap();
        assert_eq!((stats.hits, stats.misses, stats.current_size), (0, 0, 0));
    }
}
