use super::cmd;
use crate::proxy::AppState;
use bytes::BytesMut;
use redis_protocol::resp2::decode::decode_bytes;
use redis_protocol::resp2::encode::encode_bytes;
use redis_protocol::resp2::types::BytesFrame;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Handle a single RESP client connection: read frames, dispatch commands, write responses.
pub async fn handle_connection(mut stream: TcpStream, state: &AppState) {
    let mut buf = BytesMut::with_capacity(4096);

    loop {
        // Read data from client
        match stream.read_buf(&mut buf).await {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(error = %e, "RESP read error");
                break;
            }
        }

        // Try to decode complete frames from the buffer
        loop {
            // clone().freeze() gives us &Bytes without copying the data
            let (frame, consumed) = match decode_bytes(&buf.clone().freeze()) {
                Ok(Some((frame, consumed))) => (frame, consumed),
                Ok(None) => break, // Need more data
                Err(e) => {
                    tracing::debug!(error = %e, "RESP decode error");
                    let err_frame = BytesFrame::Error("ERR protocol error".into());
                    let mut out = BytesMut::new();
                    // false = don't encode integers as bulk strings (standard RESP2)
                    if encode_bytes(&mut out, &err_frame, false).is_ok() {
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
            if let Err(e) = encode_bytes(&mut out, &response, false) {
                tracing::debug!(error = %e, "RESP encode error");
                break;
            }
            if let Err(e) = stream.write_all(&out).await {
                tracing::debug!(error = %e, "RESP write error");
                return;
            }
        }
    }
}
