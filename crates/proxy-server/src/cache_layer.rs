use colander_cache::fifo::FifoCache;
use colander_cache::lru::LruCache;
use colander_cache::sharded::ShardedCache;
use colander_cache::sieve::SieveCache;
use colander_cache::traits::{CacheStats, CachedResponse};

use bytes::Bytes;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn raw_key(key: &[u8]) -> String {
    use std::fmt::Write;
    let mut encoded = String::from("\0resp:");
    for byte in key {
        write!(encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

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
                comp.get(key).is_some()
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
                comp.insert(key.clone(), value.clone());
            }
        }
        self.primary.insert(key, value);
    }

    /// RESP keys are binary-safe and isolated from HTTP cache keys.
    pub fn get_raw(&self, key: &[u8]) -> Option<Arc<CachedResponse>> {
        self.primary.get(&raw_key(key))
    }

    pub fn remove_raw(&self, key: &[u8]) -> bool {
        let key = raw_key(key);
        // Expired entries count as absent, as they do for GET.
        self.primary.get(&key).is_some() && self.primary.remove(&key)
    }

    /// SET without an expiration is persistent until eviction or deletion.
    pub fn insert_raw(&self, key: &[u8], value: Bytes, ttl: Option<Duration>) {
        let response = CachedResponse {
            status: 0,
            headers: vec![],
            body: value,
            inserted_at: Instant::now(),
            ttl: ttl.unwrap_or(Duration::MAX),
        };
        self.primary.insert(raw_key(key), response);
    }

    pub fn raw_ttl(&self, key: &[u8]) -> i64 {
        match self.get_raw(key) {
            None => -2,
            Some(entry) if entry.ttl == Duration::MAX => -1,
            Some(entry) => entry
                .ttl
                .saturating_sub(entry.inserted_at.elapsed())
                .as_secs()
                .min(i64::MAX as u64) as i64,
        }
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

    /// Invalidate both policy copies after a successful HTTP mutation.
    pub fn invalidate_http(&self, key: &str) {
        self.primary.remove(key);
        if let Some(comparison) = &self.comparison {
            comparison.remove(key);
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

    let mut max_age = None;
    let mut shared_max_age = None;
    for directive in value.split(',') {
        let (name, argument) = directive
            .trim()
            .split_once('=')
            .map_or((directive.trim(), None), |(name, value)| {
                (name.trim(), Some(value.trim().trim_matches('"')))
            });
        if ["no-store", "no-cache", "private"]
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate))
        {
            result.cacheable = false;
        }
        if name.eq_ignore_ascii_case("max-age") || name.eq_ignore_ascii_case("s-maxage") {
            let target = if name.eq_ignore_ascii_case("s-maxage") {
                &mut shared_max_age
            } else {
                &mut max_age
            };
            match argument.and_then(|value| value.parse::<u64>().ok()) {
                Some(seconds) if target.is_none() => *target = Some(Duration::from_secs(seconds)),
                _ => result.cacheable = false, // Invalid or duplicate freshness is unsafe to reuse.
            }
        }
    }
    result.max_age = shared_max_age.or(max_age);

    result
}

pub struct CacheControl {
    pub cacheable: bool,
    pub max_age: Option<Duration>,
}
