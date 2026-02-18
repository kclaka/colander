use axum::extract::Path;
use axum::routing::get;
use axum::{Json, Router};
use rand::Rng;
use serde_json::{json, Value};
use std::time::Duration;
use tracing_subscriber::EnvFilter;

async fn get_item(Path(id): Path<u64>) -> Json<Value> {
    // Simulate upstream latency (5-20ms)
    let delay = rand::thread_rng().gen_range(5..=20);
    tokio::time::sleep(Duration::from_millis(delay)).await;

    Json(json!({
        "id": id,
        "name": format!("Item {}", id),
        "data": "x".repeat(256),
        "latency_ms": delay,
    }))
}

async fn health() -> &'static str {
    "ok"
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let app = Router::new()
        .route("/api/items/{id}", get(get_item))
        .route("/health", get(health));

    let addr = "0.0.0.0:3000";
    tracing::info!(addr, "demo backend starting");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
