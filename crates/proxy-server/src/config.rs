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
    #[serde(default)]
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
    #[serde(default = "default_upstream_url")]
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
        config.validate()?;
        Ok(config)
    }

    pub fn default_config() -> Self {
        Self {
            server: ServerConfig::default(),
            upstream: UpstreamConfig::default(),
            cache: CacheConfig::default(),
            resp: RespConfig::default(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        for policy in std::iter::once(self.cache.eviction_policy.as_str())
            .chain(self.cache.comparison_policy.as_deref())
        {
            if !matches!(policy, "sieve" | "lru" | "fifo") {
                return Err(format!("unknown eviction policy: {policy}"));
            }
        }
        if self.cache.capacity == 0 || self.cache.capacity > u32::MAX as usize {
            return Err("cache capacity must be between 1 and u32::MAX".into());
        }
        if self.upstream.timeout_ms == 0 {
            return Err("upstream timeout_ms must be positive".into());
        }
        let uri: axum::http::Uri = self
            .upstream
            .url
            .parse()
            .map_err(|_| "invalid upstream URL")?;
        if uri.scheme_str() != Some("http")
            || uri.host().is_none()
            || uri.query().is_some()
            || self.upstream.url.contains('#')
            || uri.authority().is_some_and(|a| a.as_str().contains('@'))
        {
            return Err(
                "upstream URL must be an absolute http URL without credentials, query, or fragment"
                    .into(),
            );
        }
        for address in [&self.server.listen_addr, &self.server.metrics_addr]
            .into_iter()
            .chain(self.resp.enabled.then_some(&self.resp.listen_addr))
        {
            address
                .parse::<std::net::SocketAddr>()
                .map_err(|_| format!("invalid listen address: {address}"))?;
        }
        Ok(())
    }
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            url: default_upstream_url(),
            timeout_ms: default_timeout_ms(),
        }
    }
}

fn default_upstream_url() -> String {
    "http://127.0.0.1:3000".into()
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

/// Apply supported changes and retain the effective values of restart-only settings.
/// Returns the configuration actually running, so a later reload cannot accidentally
/// apply an earlier rejected capacity or body-limit change.
pub fn diff_and_apply(old: &Config, new: &Config, cache_swap: &ArcSwap<CacheLayer>) -> Config {
    if let Err(error) = new.validate() {
        tracing::error!(%error, "invalid configuration; keeping current settings");
        return old.clone();
    }
    let mut effective = old.clone();
    if old.cache.capacity != new.cache.capacity
        || old.cache.max_body_size_bytes != new.cache.max_body_size_bytes
        || old.server.listen_addr != new.server.listen_addr
        || old.server.metrics_addr != new.server.metrics_addr
        || old.upstream.url != new.upstream.url
        || old.upstream.timeout_ms != new.upstream.timeout_ms
        || old.resp.enabled != new.resp.enabled
        || old.resp.listen_addr != new.resp.listen_addr
    {
        tracing::warn!("restart-only settings changed; keeping active values until restart");
    }
    effective.cache.default_ttl_seconds = new.cache.default_ttl_seconds;
    effective.cache.eviction_policy = new.cache.eviction_policy.clone();
    effective.cache.comparison_policy = new.cache.comparison_policy.clone();

    let current = cache_swap.load_full();
    if old.cache.eviction_policy != new.cache.eviction_policy
        || old.cache.comparison_policy != new.cache.comparison_policy
    {
        let replacement = CacheLayer::new(
            &effective.cache.eviction_policy,
            effective.cache.comparison_policy.as_deref(),
            effective.cache.capacity,
            Duration::from_secs(effective.cache.default_ttl_seconds),
            effective.cache.max_body_size_bytes,
        );
        replacement.set_mode(current.mode());
        cache_swap.store(Arc::new(replacement));
        tracing::info!("eviction policies reloaded; cache cleared, mode preserved");
    } else {
        current.set_default_ttl(effective.cache.default_ttl_seconds);
    }
    effective
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_layer::CacheMode;
    use bytes::Bytes;

    #[test]
    fn partial_configs_use_upstream_defaults() {
        for input in ["", "[upstream]", "[cache]\ncapacity = 64"] {
            let config: Config = toml::from_str(input).unwrap();
            config.validate().unwrap();
            assert_eq!(config.upstream.url, default_upstream_url());
        }
    }

    #[test]
    fn rejects_invalid_runtime_settings() {
        let mut config = Config::default_config();
        config.cache.eviction_policy = "typo".into();
        assert!(config.validate().is_err());
        config = Config::default_config();
        config.cache.capacity = 0;
        assert!(config.validate().is_err());
        config = Config::default_config();
        config.upstream.timeout_ms = 0;
        assert!(config.validate().is_err());
        for url in [
            "https://example.com",
            "/relative",
            "http://example.com?a=1",
            "http://user:pass@example.com",
            "http://example.com/#x",
        ] {
            config = Config::default_config();
            config.upstream.url = url.into();
            assert!(config.validate().is_err(), "{url}");
        }
    }

    #[test]
    fn consecutive_reloads_keep_restart_only_values_and_mode() {
        let old = Config::default_config();
        let cache = ArcSwap::from(Arc::new(CacheLayer::new(
            "sieve",
            Some("lru"),
            old.cache.capacity,
            Duration::from_secs(60),
            old.cache.max_body_size_bytes,
        )));
        cache.load().set_mode(CacheMode::Bench);
        let mut edited = old.clone();
        edited.cache.capacity *= 2;
        edited.cache.max_body_size_bytes *= 2;
        let effective = diff_and_apply(&old, &edited, &cache);
        assert_eq!(effective.cache.capacity, old.cache.capacity);
        edited.cache.eviction_policy = "fifo".into();
        let effective = diff_and_apply(&effective, &edited, &cache);
        assert_eq!(effective.cache.capacity, old.cache.capacity);
        assert_eq!(cache.load().max_body_size, old.cache.max_body_size_bytes);
        assert_eq!(cache.load().mode(), CacheMode::Bench);
        assert_eq!(cache.load().primary_name(), "FIFO");
    }

    #[test]
    fn ttl_reload_preserves_data_and_invalid_policy_preserves_cache() {
        let old = Config::default_config();
        let cache = ArcSwap::from(Arc::new(CacheLayer::new(
            "sieve",
            None,
            64,
            Duration::from_secs(60),
            1024,
        )));
        let original = cache.load_full();
        original.insert(
            "key".into(),
            original.build_response(200, vec![], Bytes::from_static(b"value"), None),
        );
        let mut edited = old.clone();
        edited.cache.default_ttl_seconds = 30;
        let effective = diff_and_apply(&old, &edited, &cache);
        assert!(Arc::ptr_eq(&original, &cache.load_full()));
        assert!(cache.load().get("key").is_hit());
        assert_eq!(cache.load().default_ttl(), Duration::from_secs(30));
        edited.cache.eviction_policy = "typo".into();
        let effective = diff_and_apply(&effective, &edited, &cache);
        assert_eq!(effective.cache.eviction_policy, "sieve");
        assert!(Arc::ptr_eq(&original, &cache.load_full()));
    }
}
