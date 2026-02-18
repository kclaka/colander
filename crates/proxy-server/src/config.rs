use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub upstream: UpstreamConfig,
    #[serde(default)]
    pub cache: CacheConfig,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
    #[serde(default = "default_metrics_addr")]
    pub metrics_addr: String,
}

#[derive(Debug, Deserialize)]
pub struct UpstreamConfig {
    pub url: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
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
