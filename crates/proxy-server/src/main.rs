mod cache_layer;
mod config;
mod metrics;
mod proxy;

use axum::routing::{any, get, post};
use axum::Router;
use cache_layer::CacheLayer;
use config::Config;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use metrics::{
    metrics_broadcaster, set_mode_handler, stats_handler, ws_metrics_handler, MetricsState,
};
use proxy::{proxy_handler, AppState};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();

    // Load config
    let config = if Path::new("config.toml").exists() {
        match Config::load(Path::new("config.toml")) {
            Ok(c) => {
                tracing::info!("loaded config from config.toml");
                c
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to load config.toml, using defaults");
                Config::default_config()
            }
        }
    } else {
        tracing::info!("no config.toml found, using defaults");
        Config::default_config()
    };

    // Build cache layer
    let cache = CacheLayer::new(
        &config.cache.eviction_policy,
        config.cache.comparison_policy.as_deref(),
        config.cache.capacity,
        Duration::from_secs(config.cache.default_ttl_seconds),
        config.cache.max_body_size_bytes,
    );

    // Build HTTP client for upstream requests
    let client = Client::builder(TokioExecutor::new()).build_http();

    let state = Arc::new(AppState {
        cache,
        client,
        upstream_url: config.upstream.url.clone(),
    });

    // Metrics broadcast channel
    let (metrics_tx, _) = broadcast::channel::<metrics::MetricsSnapshot>(64);

    // Start metrics broadcaster
    let start_time = std::time::Instant::now();
    tokio::spawn(metrics_broadcaster(
        Arc::clone(&state),
        metrics_tx.clone(),
        start_time,
    ));

    // Combined metrics state
    let metrics_state = MetricsState {
        app: Arc::clone(&state),
        tx: metrics_tx,
    };

    // Build metrics/admin router (separate port)
    let metrics_router = Router::new()
        .route("/ws/metrics", get(ws_metrics_handler))
        .route("/api/mode", post(set_mode_handler))
        .route("/api/stats", get(stats_handler))
        .with_state(metrics_state);

    // Build proxy router (main port)
    let proxy_router = Router::new()
        .route("/{*path}", any(proxy_handler))
        .route("/", any(proxy_handler))
        .with_state(Arc::clone(&state));

    // Start both servers
    let proxy_addr = config.server.listen_addr.clone();
    let metrics_addr = config.server.metrics_addr.clone();

    tracing::info!(
        proxy = %proxy_addr,
        metrics = %metrics_addr,
        upstream = %config.upstream.url,
        policy = %config.cache.eviction_policy,
        comparison = ?config.cache.comparison_policy,
        capacity = config.cache.capacity,
        "colander proxy starting"
    );

    let proxy_listener = tokio::net::TcpListener::bind(&proxy_addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind proxy to {proxy_addr}: {e}"));

    let metrics_listener = tokio::net::TcpListener::bind(&metrics_addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind metrics to {metrics_addr}: {e}"));

    tokio::select! {
        result = axum::serve(proxy_listener, proxy_router) => {
            if let Err(e) = result {
                tracing::error!(error = %e, "proxy server error");
            }
        }
        result = axum::serve(metrics_listener, metrics_router) => {
            if let Err(e) = result {
                tracing::error!(error = %e, "metrics server error");
            }
        }
    }
}
