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
    let mut previous_cache: Option<Arc<CacheLayer>> = None;
    let mut previous_primary: Option<PolicyMetrics> = None;
    let mut previous_comparison: Option<PolicyMetrics> = None;
    let mut previous_sample = Instant::now();
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        let cache = state.cache.load_full();
        let primary = PolicyMetrics::from_cache(&cache, true).unwrap(); // primary always Some
        let comparison = PolicyMetrics::from_cache(&cache, false);

        let now = Instant::now();
        let window = now.duration_since(previous_sample);
        previous_sample = now;
        let same_cache = previous_cache
            .as_ref()
            .is_some_and(|previous| Arc::ptr_eq(previous, &cache));
        if !same_cache {
            for (old, role) in [
                (previous_primary.as_ref(), "primary"),
                (previous_comparison.as_ref(), "comparison"),
            ] {
                if let Some(old) = old {
                    ::metrics::gauge!("colander_cache_keys", "policy" => old.name.clone(), "role" => role).set(0.0);
                    ::metrics::gauge!("colander_cache_capacity", "policy" => old.name.clone(), "role" => role).set(0.0);
                }
            }
            previous_primary = None;
            previous_comparison = None;
        }
        let throughput = lookup_rate(&primary, previous_primary.as_ref(), window);
        publish_policy(&primary, previous_primary.as_ref(), "primary");
        if let Some(comparison) = &comparison {
            publish_policy(comparison, previous_comparison.as_ref(), "comparison");
        }
        ::metrics::gauge!("colander_cache_lookups_per_second").set(throughput);
        previous_cache = Some(cache.clone());
        previous_primary = Some(primary.clone());
        previous_comparison = comparison.clone();

        let snapshot = MetricsSnapshot {
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap() // safe: clock is after 1970
                .as_millis(),
            window_ms: window.as_millis().max(1) as u64,
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

fn lookup_rate(
    current: &PolicyMetrics,
    previous: Option<&PolicyMetrics>,
    window: std::time::Duration,
) -> f64 {
    let old = previous.map_or(0, |p| p.hits + p.misses);
    let count = (current.hits + current.misses).saturating_sub(old);
    count as f64 / window.as_secs_f64().max(0.001)
}

fn publish_policy(current: &PolicyMetrics, previous: Option<&PolicyMetrics>, role: &'static str) {
    for (metric, value, old) in [
        (
            "colander_cache_hits_total",
            current.hits,
            previous.map_or(0, |p| p.hits),
        ),
        (
            "colander_cache_misses_total",
            current.misses,
            previous.map_or(0, |p| p.misses),
        ),
        (
            "colander_cache_evictions_total",
            current.evictions,
            previous.map_or(0, |p| p.evictions),
        ),
    ] {
        ::metrics::counter!(metric, "policy" => current.name.clone(), "role" => role)
            .increment(value.saturating_sub(old));
    }
    ::metrics::gauge!("colander_cache_keys", "policy" => current.name.clone(), "role" => role)
        .set(current.size as f64);
    ::metrics::gauge!("colander_cache_capacity", "policy" => current.name.clone(), "role" => role)
        .set(current.capacity as f64);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sample(hits: u64) -> PolicyMetrics {
        PolicyMetrics {
            name: "SIEVE".into(),
            hits,
            misses: 0,
            evictions: 0,
            hit_rate: 1.0,
            size: 2,
            capacity: 64,
        }
    }

    #[test]
    fn uses_actual_window_and_resets_baseline_after_cache_replacement() {
        assert_eq!(
            lookup_rate(&sample(10), Some(&sample(4)), Duration::from_secs(2)),
            3.0
        );
        assert_eq!(
            lookup_rate(&sample(2), None, Duration::from_millis(500)),
            4.0
        );
    }

    #[test]
    fn prometheus_exports_real_counters_and_gauges_without_double_counting() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        ::metrics::with_local_recorder(&recorder, || {
            publish_policy(&sample(4), None, "primary");
            publish_policy(&sample(6), Some(&sample(4)), "primary");
            publish_policy(&sample(6), Some(&sample(6)), "primary");
            // A replaced cache adds its new counts to the monotonic series.
            publish_policy(&sample(2), None, "primary");
        });
        let output = handle.render();
        assert!(
            output.contains("colander_cache_hits_total{policy=\"SIEVE\",role=\"primary\"} 8"),
            "{output}"
        );
        assert!(
            output.contains("colander_cache_keys{policy=\"SIEVE\",role=\"primary\"} 2"),
            "{output}"
        );
        assert!(
            output.contains("colander_cache_capacity{policy=\"SIEVE\",role=\"primary\"} 64"),
            "{output}"
        );
    }
}
