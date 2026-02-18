mod cache_layer;
mod config;
mod metrics;
mod proxy;
mod resp;

use arc_swap::ArcSwap;
use axum::routing::{any, get, post};
use axum::Router;
use cache_layer::CacheLayer;
use config::Config;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use metrics::{
    metrics_broadcaster, set_mode_handler, stats_handler, ws_metrics_handler, MetricsState,
};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use parking_lot::Mutex;
use proxy::{proxy_handler, AppState};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
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

    // Install Prometheus metrics recorder
    let prom_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .expect("prometheus recorder");

    // Build cache layer (wrapped in ArcSwap for hot-reload)
    let cache = Arc::new(CacheLayer::new(
        &config.cache.eviction_policy,
        config.cache.comparison_policy.as_deref(),
        config.cache.capacity,
        Duration::from_secs(config.cache.default_ttl_seconds),
        config.cache.max_body_size_bytes,
    ));

    let cache_swap = Arc::new(ArcSwap::from(cache));

    // Build HTTP client for upstream requests
    let client = Client::builder(TokioExecutor::new()).build_http();

    let state = Arc::new(AppState {
        cache: ArcSwap::from(cache_swap.load_full()),
        client,
        upstream_url: config.upstream.url.clone(),
    });

    // Shutdown token for graceful shutdown
    let shutdown = CancellationToken::new();

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
        .route(
            "/metrics",
            get(move || {
                let h = prom_handle.clone();
                async move { h.render() }
            }),
        )
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
        resp_enabled = config.resp.enabled,
        "colander proxy starting"
    );

    let proxy_listener = tokio::net::TcpListener::bind(&proxy_addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind proxy to {proxy_addr}: {e}"));

    let metrics_listener = tokio::net::TcpListener::bind(&metrics_addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind metrics to {metrics_addr}: {e}"));

    // Spawn RESP server if enabled
    if config.resp.enabled {
        let resp_addr = config.resp.listen_addr.clone();
        let resp_cache = Arc::clone(&state);
        let resp_shutdown = shutdown.clone();
        tokio::spawn(async move {
            resp::run_resp_server(&resp_addr, resp_cache, resp_shutdown).await;
        });
    }

    // Spawn config file watcher
    spawn_config_watcher(PathBuf::from("config.toml"), config, Arc::clone(&state));

    // Spawn shutdown signal handler
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        shutdown_signal(shutdown_clone).await;
    });

    // Run both servers with graceful shutdown
    let proxy_shutdown = shutdown.clone();
    let metrics_shutdown = shutdown.clone();

    let proxy_future = axum::serve(proxy_listener, proxy_router)
        .with_graceful_shutdown(proxy_shutdown.cancelled_owned());

    let metrics_future = axum::serve(metrics_listener, metrics_router)
        .with_graceful_shutdown(metrics_shutdown.cancelled_owned());

    tokio::select! {
        result = proxy_future => {
            if let Err(e) = result {
                tracing::error!(error = %e, "proxy server error");
            }
        }
        result = metrics_future => {
            if let Err(e) = result {
                tracing::error!(error = %e, "metrics server error");
            }
        }
    }

    tracing::info!("colander proxy shut down");
}

/// Listen for SIGINT (Ctrl+C) or SIGTERM and cancel the shutdown token.
async fn shutdown_signal(token: CancellationToken) {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = sigterm.recv() => {},
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await.ok();
    }

    tracing::info!("shutdown signal received, draining connections...");
    token.cancel();
}

/// Spawn a filesystem watcher on config.toml that applies safe config changes at runtime.
fn spawn_config_watcher(config_path: PathBuf, initial_config: Config, state: Arc<AppState>) {
    let current_config = Arc::new(Mutex::new(initial_config));

    let config_path_clone = config_path.clone();
    let mut watcher = match notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                match Config::load(&config_path_clone) {
                    Ok(new_config) => {
                        let mut old = current_config.lock();
                        config::diff_and_apply(&old, &new_config, &state.cache);
                        *old = new_config;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "failed to reload config.toml");
                    }
                }
            }
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "failed to start config watcher");
            return;
        }
    };

    if let Err(e) = watcher.watch(&config_path, RecursiveMode::NonRecursive) {
        tracing::warn!(error = %e, "failed to watch config.toml");
        return;
    }

    // Leak the watcher so it lives for the process lifetime
    std::mem::forget(watcher);
    tracing::info!("config file watcher started");
}
