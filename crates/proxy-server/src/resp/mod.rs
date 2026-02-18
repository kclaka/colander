mod cmd;
mod connection;

use crate::proxy::AppState;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Run the RESP2 server on the given address, sharing the same cache as the HTTP proxy.
pub async fn run_resp_server(addr: &str, state: Arc<AppState>, shutdown: CancellationToken) {
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => {
            tracing::info!(addr = %addr, "RESP server listening");
            l
        }
        Err(e) => {
            tracing::error!(error = %e, addr = %addr, "failed to bind RESP server");
            return;
        }
    };

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!("RESP server shutting down");
                break;
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, peer)) => {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move {
                            tracing::debug!(peer = %peer, "RESP client connected");
                            connection::handle_connection(stream, &state).await;
                            tracing::debug!(peer = %peer, "RESP client disconnected");
                        });
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "RESP accept error");
                    }
                }
            }
        }
    }
}
