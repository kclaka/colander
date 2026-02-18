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
    #[arg(long, default_value_t = 100_000)]
    num_items: u64,

    /// Number of concurrent request tasks
    #[arg(long, default_value_t = 16)]
    concurrency: u64,

    /// Target requests per second (0 = unlimited)
    #[arg(long, default_value_t = 0)]
    rps: u64,

    /// Initial Zipfian alpha (skewness)
    #[arg(long, default_value_t = 0.8)]
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

async fn control_handler(
    State(state): State<Arc<LoadGenState>>,
    Json(body): Json<ControlRequest>,
) -> Json<ControlResponse> {
    if let Some(alpha) = body.alpha {
        let clamped = alpha.clamp(0.01, 3.0);
        state.set_alpha(clamped);
        tracing::info!(alpha = clamped, "alpha updated");
    }
    if let Some(running) = body.running {
        state.running.store(running, Ordering::Relaxed);
        tracing::info!(running, "running state updated");
    }

    Json(ControlResponse {
        alpha: state.alpha(),
        running: state.running.load(Ordering::Relaxed),
        total_requests: state.total_requests.load(Ordering::Relaxed),
    })
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
    let delay = if state.rps > 0 {
        let per_worker_rps = state.rps / state.concurrency.max(1);
        if per_worker_rps > 0 {
            Some(Duration::from_micros(1_000_000 / per_worker_rps))
        } else {
            None
        }
    } else {
        None
    };

    // Each worker gets its own generator (rand is not Send-safe across awaits with thread_rng)
    let mut gen = ZipfianGenerator::new(state.num_items, state.alpha());

    loop {
        if !state.running.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }

        // Check if alpha changed and rebuild generator
        let current_alpha = state.alpha();
        if (current_alpha - gen.alpha()).abs() > 0.001 {
            gen = ZipfianGenerator::new(state.num_items, current_alpha);
        }

        let item_id = gen.next_id();
        let url = format!("{}/api/items/{}", state.proxy_url, item_id);

        match client.get(&url).send().await {
            Ok(_resp) => {
                state.total_requests.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                if worker_id == 0 {
                    tracing::warn!(error = %e, "request failed");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        if let Some(d) = delay {
            tokio::time::sleep(d).await;
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
