mod zipfian;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::EnvFilter;
use zipfian::ZipfianGenerator;

/// Colander load generator — Zipfian traffic for cache benchmarking.
#[derive(Parser)]
#[command(name = "loadgen")]
struct Args {
    /// Target proxy URL
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    proxy_url: String,

    /// Number of unique items in the dataset
    #[arg(long, default_value_t = 100_000, value_parser = clap::value_parser!(u64).range(1..))]
    num_items: u64,

    /// Number of concurrent request tasks
    #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u64).range(1..))]
    concurrency: u64,

    /// Target requests per second (0 = unlimited)
    #[arg(long, default_value_t = 0)]
    rps: u64,

    /// Initial Zipfian alpha (skewness)
    #[arg(long, default_value_t = 0.8, value_parser = parse_alpha)]
    alpha: f64,

    /// Control server listen address
    #[arg(long, default_value = "0.0.0.0:9091")]
    control_addr: String,
}

/// Shared state for the load generator.
struct LoadGenState {
    /// Zipfian alpha stored as fixed-point (alpha * 1000) for lock-free updates.
    alpha_fp: AtomicU64,
    num_items: u64,
    running: AtomicBool,
    proxy_url: String,
    rps: u64,
    concurrency: u64,
    /// Total requests sent (atomic counter).
    total_requests: AtomicU64,
    limiter: Option<RateLimiter>,
}

impl LoadGenState {
    fn alpha(&self) -> f64 {
        self.alpha_fp.load(Ordering::Relaxed) as f64 / 1000.0
    }

    fn set_alpha(&self, alpha: f64) {
        let fp = (alpha * 1000.0) as u64;
        self.alpha_fp.store(fp, Ordering::Relaxed);
    }
}

#[derive(Deserialize)]
struct ControlRequest {
    #[serde(default)]
    alpha: Option<f64>,
    #[serde(default)]
    running: Option<bool>,
}

#[derive(Serialize)]
struct ControlResponse {
    alpha: f64,
    running: bool,
    total_requests: u64,
}

#[derive(Serialize)]
struct StatusResponse {
    alpha: f64,
    running: bool,
    total_requests: u64,
    num_items: u64,
    concurrency: u64,
    rps: u64,
}

fn parse_alpha(value: &str) -> Result<f64, String> {
    let alpha = value.parse::<f64>().map_err(|_| "alpha must be a number")?;
    if !alpha.is_finite() || !(0.01..=3.0).contains(&alpha) {
        return Err("alpha must be finite and between 0.01 and 3.0".into());
    }
    Ok(alpha)
}

async fn control_handler(
    State(state): State<Arc<LoadGenState>>,
    Json(body): Json<ControlRequest>,
) -> Result<Json<ControlResponse>, (axum::http::StatusCode, String)> {
    if let Some(alpha) = body.alpha {
        parse_alpha(&alpha.to_string())
            .map_err(|error| (axum::http::StatusCode::BAD_REQUEST, error))?;
        state.set_alpha(alpha);
        tracing::info!(alpha, "alpha updated");
    }
    if let Some(running) = body.running {
        state.running.store(running, Ordering::Relaxed);
    }
    Ok(Json(ControlResponse {
        alpha: state.alpha(),
        running: state.running.load(Ordering::Relaxed),
        total_requests: state.total_requests.load(Ordering::Relaxed),
    }))
}

/// One shared schedule enforces the aggregate rate, even below worker count.
struct RateLimiter {
    next: tokio::sync::Mutex<tokio::time::Instant>,
    period: Duration,
}

impl RateLimiter {
    fn new(rps: u64) -> Option<Self> {
        (rps > 0).then(|| Self {
            next: tokio::sync::Mutex::new(tokio::time::Instant::now()),
            period: Duration::from_secs_f64(1.0 / rps as f64).max(Duration::from_nanos(1)),
        })
    }

    async fn wait(&self) {
        let mut next = self.next.lock().await;
        tokio::time::sleep_until(*next).await;
        // Schedule from now: slow requests and pauses cannot accumulate burst credits.
        *next = tokio::time::Instant::now() + self.period;
    }
}

async fn status_handler(State(state): State<Arc<LoadGenState>>) -> Json<StatusResponse> {
    Json(StatusResponse {
        alpha: state.alpha(),
        running: state.running.load(Ordering::Relaxed),
        total_requests: state.total_requests.load(Ordering::Relaxed),
        num_items: state.num_items,
        concurrency: state.concurrency,
        rps: state.rps,
    })
}

/// Worker task that sends requests to the proxy using a Zipfian distribution.
async fn worker(state: Arc<LoadGenState>, client: Client, worker_id: u64) {
    // Each worker gets its own generator (rand is not Send-safe across awaits with thread_rng)
    let mut gen = ZipfianGenerator::new(state.num_items, state.alpha());

    loop {
        if !state.running.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }

        if let Some(limiter) = &state.limiter {
            limiter.wait().await;
            if !state.running.load(Ordering::Relaxed) {
                continue;
            }
        }

        // Check if alpha changed and rebuild generator
        let current_alpha = state.alpha();
        if current_alpha != gen.alpha() {
            gen = ZipfianGenerator::new(state.num_items, current_alpha);
        }

        let item_id = gen.next_id();
        let url = format!("{}/api/items/{}", state.proxy_url, item_id);

        match client.get(&url).send().await {
            Ok(mut response) => {
                // Drain the body so pooled connections can be reused. Count only
                // complete requests, without allocating the whole response body.
                loop {
                    match response.chunk().await {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            state.total_requests.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                        Err(error) => {
                            tracing::debug!(%error, "response body failed");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                if worker_id == 0 {
                    tracing::warn!(error = %e, "request failed");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let state = Arc::new(LoadGenState {
        alpha_fp: AtomicU64::new((args.alpha * 1000.0) as u64),
        num_items: args.num_items,
        running: AtomicBool::new(true),
        proxy_url: args.proxy_url.clone(),
        rps: args.rps,
        concurrency: args.concurrency,
        total_requests: AtomicU64::new(0),
        limiter: RateLimiter::new(args.rps),
    });

    // Build control server
    let control_router = Router::new()
        .route("/control", post(control_handler))
        .route("/status", get(status_handler))
        .with_state(Arc::clone(&state));

    let control_addr = args.control_addr.clone();

    tracing::info!(
        proxy = %args.proxy_url,
        alpha = args.alpha,
        num_items = args.num_items,
        concurrency = args.concurrency,
        rps = args.rps,
        control = %control_addr,
        "loadgen starting"
    );

    // Spawn control server
    let control_listener = tokio::net::TcpListener::bind(&control_addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind control server to {control_addr}: {e}"));

    tokio::spawn(async move {
        if let Err(e) = axum::serve(control_listener, control_router).await {
            tracing::error!(error = %e, "control server error");
        }
    });

    // Build HTTP client for proxy requests
    let client = Client::builder()
        .pool_max_idle_per_host(64)
        .timeout(Duration::from_secs(5))
        .build()
        .expect("failed to build HTTP client");

    // Spawn workers
    let mut handles = Vec::new();
    for i in 0..args.concurrency {
        let s = Arc::clone(&state);
        let c = client.clone();
        handles.push(tokio::spawn(worker(s, c, i)));
    }

    // Log throughput every 5 seconds
    let stats_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut prev = 0u64;
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            let current = stats_state.total_requests.load(Ordering::Relaxed);
            let delta = current - prev;
            let rps = delta as f64 / 5.0;
            prev = current;
            tracing::info!(
                total = current,
                rps = format!("{:.0}", rps),
                alpha = format!("{:.2}", stats_state.alpha()),
                "throughput"
            );
        }
    });

    // Wait for all workers (runs forever)
    for h in handles {
        let _ = h.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_cli_before_starting_workers() {
        for args in [
            vec!["loadgen", "--num-items=0"],
            vec!["loadgen", "--concurrency=0"],
            vec!["loadgen", "--alpha=NaN"],
            vec!["loadgen", "--alpha=-1"],
            vec!["loadgen", "--alpha=4"],
        ] {
            assert!(Args::try_parse_from(args).is_err());
        }
        assert!(Args::try_parse_from(["loadgen", "--rps=1", "--concurrency=16"]).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn aggregate_rate_is_bounded_below_concurrency() {
        let limiter = Arc::new(RateLimiter::new(2).unwrap());
        let start = tokio::time::Instant::now();
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let limiter = Arc::clone(&limiter);
            tasks.push(tokio::spawn(async move {
                limiter.wait().await;
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert!(start.elapsed() >= Duration::from_millis(7500));
        assert!(start.elapsed() < Duration::from_secs(9));
        assert!(RateLimiter::new(0).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_pause_does_not_accumulate_burst_credit() {
        let limiter = RateLimiter::new(10).unwrap();
        limiter.wait().await;
        tokio::time::advance(Duration::from_secs(30)).await;
        limiter.wait().await;
        let resumed = tokio::time::Instant::now();
        limiter.wait().await;
        assert!(resumed.elapsed() >= Duration::from_millis(100));
    }

    #[tokio::test]
    async fn invalid_control_is_rejected_without_partial_updates() {
        let state = Arc::new(LoadGenState {
            alpha_fp: AtomicU64::new(800),
            num_items: 10,
            running: AtomicBool::new(true),
            proxy_url: String::new(),
            rps: 0,
            concurrency: 1,
            total_requests: AtomicU64::new(0),
            limiter: None,
        });
        let result = control_handler(
            State(Arc::clone(&state)),
            Json(ControlRequest {
                alpha: Some(-1.0),
                running: Some(false),
            }),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(state.alpha(), 0.8);
        assert!(state.running.load(Ordering::Relaxed));
    }
}
