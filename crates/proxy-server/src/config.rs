use crate::cache_layer::CacheLayer;
use arc_swap::ArcSwap;
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub upstream: UpstreamConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub resp: RespConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
    #[serde(default = "default_metrics_addr")]
    pub metrics_addr: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpstreamConfig {
    pub url: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheConfig {
    #[serde(default = "default_capacity")]
    pub capacity: usize,
    #[serde(default = "default_ttl")]
    pub default_ttl_seconds: u64,
    #[serde(default = "default_max_body_size")]
    pub max_body_size_bytes: usize,
    #[serde(default = "default_eviction_policy")]
    pub eviction_policy: String,
    #[serde(default)]
    pub comparison_policy: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RespConfig {
    #[serde(default = "default_resp_enabled")]
    pub enabled: bool,
    #[serde(default = "default_resp_addr")]
    pub listen_addr: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        Ok(config)
    }

    pub fn default_config() -> Self {
        Config {
            server: ServerConfig::default(),
            upstream: UpstreamConfig {
                url: "http://127.0.0.1:3000".to_string(),
                timeout_ms: 5000,
            },
            cache: CacheConfig::default(),
            resp: RespConfig::default(),
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: default_listen_addr(),
            metrics_addr: default_metrics_addr(),
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            capacity: default_capacity(),
            default_ttl_seconds: default_ttl(),
            max_body_size_bytes: default_max_body_size(),
            eviction_policy: default_eviction_policy(),
            comparison_policy: Some("lru".to_string()),
        }
    }
}

impl Default for RespConfig {
    fn default() -> Self {
        Self {
            enabled: default_resp_enabled(),
            listen_addr: default_resp_addr(),
        }
    }
}

/// Compare old and new config, apply safe changes, reject unsafe ones.
///
/// - TTL changed → atomic update (no cache data loss)
/// - Eviction policy changed → rebuild cache (data cleared)
/// - Capacity changed → WARN log, ignore (restart required)
pub fn diff_and_apply(old: &Config, new: &Config, cache_swap: &ArcSwap<CacheLayer>) {
    // Capacity changed → WARN, ignore
    if old.cache.capacity != new.cache.capacity {
        tracing::warn!(
            old = old.cache.capacity,
            new = new.cache.capacity,
            "capacity change detected — ignoring. Restart to resize cache safely"
        );
    }

    // TTL changed → atomic update (no cache loss)
    if old.cache.default_ttl_seconds != new.cache.default_ttl_seconds {
        cache_swap
            .load()
            .set_default_ttl(new.cache.default_ttl_seconds);
        tracing::info!(
            old = old.cache.default_ttl_seconds,
            new = new.cache.default_ttl_seconds,
            "config reloaded: TTL changed"
        );
    }

    // Eviction policy changed → rebuild cache (data cleared)
    if old.cache.eviction_policy != new.cache.eviction_policy
        || old.cache.comparison_policy != new.cache.comparison_policy
    {
        let new_cache = CacheLayer::new(
            &new.cache.eviction_policy,
            new.cache.comparison_policy.as_deref(),
            old.cache.capacity, // Use OLD capacity (immutable)
            Duration::from_secs(new.cache.default_ttl_seconds),
            new.cache.max_body_size_bytes,
        );
        cache_swap.store(Arc::new(new_cache));
        tracing::info!(
            old_policy = %old.cache.eviction_policy,
            new_policy = %new.cache.eviction_policy,
            "config reloaded: eviction policy changed. Cache cleared."
        );
    }
}

fn default_listen_addr() -> String {
    "0.0.0.0:8080".to_string()
}
fn default_metrics_addr() -> String {
    "0.0.0.0:9090".to_string()
}
fn default_timeout_ms() -> u64 {
    5000
}
fn default_capacity() -> usize {
    10000
}
fn default_ttl() -> u64 {
    60
}
fn default_max_body_size() -> usize {
    1_048_576
}
fn default_eviction_policy() -> String {
    "sieve".to_string()
}
fn default_resp_enabled() -> bool {
    true
}
fn default_resp_addr() -> String {
    "0.0.0.0:6379".to_string()
}
