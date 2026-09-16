use super::cmd;
use crate::proxy::AppState;
use bytes::BytesMut;
use redis_protocol::resp2::decode::decode_bytes;
use redis_protocol::resp2::encode::extend_encode;
use redis_protocol::resp2::types::BytesFrame;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_FRAME_SIZE: usize = 8 * 1024 * 1024;

/// Handle a single RESP client connection: read frames, dispatch commands, write responses.
pub async fn handle_connection<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, state: &AppState) {
    let mut buf = BytesMut::with_capacity(4096);

    loop {
        if buf.len() >= MAX_FRAME_SIZE {
            let _ = stream
                .write_all(b"-ERR frame exceeds 8 MiB limit\r\n")
                .await;
            return;
        }
        let mut chunk = [0u8; 8192];
        let available = chunk.len().min(MAX_FRAME_SIZE - buf.len());
        match stream.read(&mut chunk[..available]).await {
            Ok(0) => break, // EOF
            Ok(read) => buf.extend_from_slice(&chunk[..read]),
            Err(e) => {
                tracing::debug!(error = %e, "RESP read error");
                break;
            }
        }

        // Try to decode complete frames from the buffer
        loop {
            // The decoder returns owned byte slices for dispatch.
            let (frame, consumed) = match decode_bytes(&buf.clone().freeze()) {
                Ok(Some((frame, consumed))) => (frame, consumed),
                Ok(None) => break, // Need more data
                Err(e) => {
                    tracing::debug!(error = %e, "RESP decode error");
                    let err_frame = BytesFrame::Error("ERR protocol error".into());
                    let mut out = BytesMut::new();
                    // false = don't encode integers as bulk strings (standard RESP2)
                    if extend_encode(&mut out, &err_frame, false).is_ok() {
                        let _ = stream.write_all(&out).await;
                    }
                    return;
                }
            };

            // Advance the buffer past the consumed bytes
            let _ = buf.split_to(consumed);

            // Dispatch the command
            let response = cmd::dispatch(&frame, state);

            // Encode and send the response
            let mut out = BytesMut::new();
            if let Err(e) = extend_encode(&mut out, &response, false) {
                tracing::debug!(error = %e, "RESP encode error");
                return;
            }
            if let Err(e) = stream.write_all(&out).await {
                tracing::debug!(error = %e, "RESP write error");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_layer::CacheLayer;
    use arc_swap::ArcSwap;
    use hyper_util::{client::legacy::Client, rt::TokioExecutor};
    use std::{sync::Arc, time::Duration};

    fn connection() -> (tokio::io::DuplexStream, tokio::task::JoinHandle<()>) {
        let (client, server) = tokio::io::duplex(4096);
        let state = AppState {
            cache: ArcSwap::from(Arc::new(CacheLayer::new(
                "sieve",
                None,
                64,
                Duration::from_secs(60),
                1024,
            ))),
            client: Client::builder(TokioExecutor::new()).build_http(),
            upstream_url: "http://127.0.0.1:1".into(),
        };
        let task = tokio::spawn(async move {
            handle_connection(server, &state).await;
        });
        (client, task)
    }

    #[tokio::test]
    async fn fragmented_and_pipelined_commands_receive_complete_encoded_replies() {
        let (mut client, task) = connection();
        client.write_all(b"*1\r\n$4\r\nPI").await.unwrap();
        tokio::task::yield_now().await;
        client
            .write_all(
                b"NG\r\n*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n*2\r\n$3\r\nGET\r\n$1\r\nk\r\n",
            )
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut output))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output, b"+PONG\r\n+OK\r\n$1\r\nv\r\n");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn protocol_errors_are_encoded_before_the_connection_closes() {
        let (mut client, task) = connection();
        client.write_all(b"?invalid\r\n").await.unwrap();
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut output))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output, b"-ERR protocol error\r\n");
        task.await.unwrap();
    }
}
