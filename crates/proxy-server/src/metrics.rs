use crate::cache_layer::{CacheLayer, CacheMode};
use crate::proxy::AppState;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;

/// Combined state for the metrics router (holds both AppState and broadcast sender).
#[derive(Clone)]
pub struct MetricsState {
    pub app: Arc<AppState>,
    pub tx: broadcast::Sender<MetricsSnapshot>,
}

/// Metrics snapshot broadcast to WebSocket clients every 500ms.
#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub timestamp_ms: u128,
    pub window_ms: u64,
    pub primary: PolicyMetrics,
    pub comparison: Option<PolicyMetrics>,
    pub throughput_rps: f64,
    pub uptime_seconds: u64,
    pub mode: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PolicyMetrics {
    pub name: String,
    pub hit_rate: f64,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub size: usize,
    pub capacity: usize,
}

impl PolicyMetrics {
    fn from_cache(cache: &CacheLayer, primary: bool) -> Option<Self> {
        if primary {
            let stats = cache.primary_stats();
            let total = stats.hits + stats.misses;
            Some(PolicyMetrics {
                name: cache.primary_name().to_string(),
                hit_rate: if total > 0 {
                    stats.hits as f64 / total as f64
                } else {
                    0.0
                },
                hits: stats.hits,
                misses: stats.misses,
                evictions: stats.evictions,
                size: stats.current_size,
                capacity: stats.capacity,
            })
        } else {
            let stats = cache.comparison_stats()?;
            let name = cache.comparison_name()?;
            let total = stats.hits + stats.misses;
            Some(PolicyMetrics {
                name: name.to_string(),
                hit_rate: if total > 0 {
                    stats.hits as f64 / total as f64
                } else {
                    0.0
                },
                hits: stats.hits,
                misses: stats.misses,
                evictions: stats.evictions,
                size: stats.current_size,
                capacity: stats.capacity,
            })
        }
    }
}

/// Background task that snapshots metrics every 500ms and broadcasts to clients.
pub async fn metrics_broadcaster(
    state: Arc<AppState>,
    tx: broadcast::Sender<MetricsSnapshot>,
    start_time: Instant,
) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));
    let mut prev_total_requests: u64 = 0;

    loop {
        interval.tick().await;

        let cache = state.cache.load();
        let primary = PolicyMetrics::from_cache(&cache, true).unwrap(); // primary always Some
        let comparison = PolicyMetrics::from_cache(&cache, false);

        let current_total = primary.hits + primary.misses;
        let delta = current_total.saturating_sub(prev_total_requests);
        let throughput = delta as f64 * 2.0; // 500ms window → multiply by 2 for per-second
        prev_total_requests = current_total;

        let snapshot = MetricsSnapshot {
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap() // safe: clock is after 1970
                .as_millis(),
            window_ms: 500,
            primary,
            comparison,
            throughput_rps: throughput,
            uptime_seconds: start_time.elapsed().as_secs(),
            mode: format!("{:?}", cache.mode()).to_lowercase(),
        };

        // Ignore send errors (no subscribers)
        let _ = tx.send(snapshot);
    }
}

/// WebSocket upgrade handler for /ws/metrics.
pub async fn ws_metrics_handler(
    ws: WebSocketUpgrade,
    State(state): State<MetricsState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_client(socket, state.tx))
}

async fn handle_ws_client(mut socket: WebSocket, tx: broadcast::Sender<MetricsSnapshot>) {
    let mut rx = tx.subscribe();

    loop {
        match rx.recv().await {
            Ok(snapshot) => {
                let json = match serde_json::to_string(&snapshot) {
                    Ok(j) => j,
                    Err(_) => continue,
                };
                if socket.send(Message::Text(json.into())).await.is_err() {
                    break; // Client disconnected
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// POST /api/mode — toggle between demo and bench mode.
#[derive(Deserialize)]
pub struct ModeRequest {
    pub mode: String,
}

pub async fn set_mode_handler(
    State(state): State<MetricsState>,
    Json(body): Json<ModeRequest>,
) -> impl IntoResponse {
    let mode = match body.mode.as_str() {
        "demo" => CacheMode::Demo,
        "bench" => CacheMode::Bench,
        other => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({"error": format!("unknown mode: {other}, use 'demo' or 'bench'")}),
                ),
            );
        }
    };

    state.app.cache.load().set_mode(mode);

    (
        axum::http::StatusCode::OK,
        Json(serde_json::json!({"mode": body.mode})),
    )
}

/// GET /api/stats — one-shot stats endpoint.
pub async fn stats_handler(State(state): State<MetricsState>) -> impl IntoResponse {
    let cache = state.app.cache.load();
    let primary = PolicyMetrics::from_cache(&cache, true);
    let comparison = PolicyMetrics::from_cache(&cache, false);

    Json(serde_json::json!({
        "primary": primary,
        "comparison": comparison,
        "mode": format!("{:?}", cache.mode()).to_lowercase(),
    }))
}
