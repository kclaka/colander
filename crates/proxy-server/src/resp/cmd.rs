use crate::proxy::AppState;
use bytes::Bytes;
use redis_protocol::resp2::types::BytesFrame;
use std::time::Duration;

/// Validate the complete command before accessing or mutating the cache.
pub fn dispatch(frame: &BytesFrame, state: &AppState) -> BytesFrame {
    let frames = match frame {
        BytesFrame::Array(args) if !args.is_empty() => args,
        _ => return error_frame("ERR expected a non-empty array"),
    };
    let args: Option<Vec<&Bytes>> = frames
        .iter()
        .map(|frame| match frame {
            BytesFrame::BulkString(bytes) => Some(bytes),
            _ => None,
        })
        .collect();
    let Some(args) = args else {
        return error_frame("ERR command arguments must be bulk strings");
    };
    let command = args[0].to_ascii_uppercase();
    let cache = state.cache.load();
    match command.as_slice() {
        b"PING" => match args.len() {
            1 => BytesFrame::SimpleString("PONG".into()),
            2 => BytesFrame::BulkString(args[1].clone()),
            _ => error_frame("ERR wrong number of arguments for 'PING' command"),
        },
        b"GET" if args.len() == 2 => cache
            .get_raw(args[1])
            .map(|entry| BytesFrame::BulkString(entry.body.clone()))
            .unwrap_or(BytesFrame::Null),
        b"SET" if args.len() >= 3 => {
            let ttl = match parse_set_options(&args[3..]) {
                Ok(ttl) => ttl,
                Err(error) => return error_frame(error),
            };
            if args[2].len() > cache.max_body_size {
                return error_frame("ERR value exceeds configured maximum body size");
            }
            cache.insert_raw(args[1], args[2].clone(), ttl);
            BytesFrame::SimpleString("OK".into())
        }
        b"DEL" if args.len() >= 2 => {
            let count = args[1..].iter().filter(|key| cache.remove_raw(key)).count();
            BytesFrame::Integer(count as i64)
        }
        b"TTL" if args.len() == 2 => BytesFrame::Integer(cache.raw_ttl(args[1])),
        b"GET" | b"SET" | b"DEL" | b"TTL" => error_frame("ERR wrong number of arguments"),
        b"EXPIRE" => error_frame("ERR EXPIRE is not supported; use SET with EX or PX"),
        _ => error_frame("ERR unknown or unsupported command"),
    }
}

fn error_frame(message: &str) -> BytesFrame {
    BytesFrame::Error(message.into())
}

fn parse_set_options(args: &[&Bytes]) -> Result<Option<Duration>, &'static str> {
    if args.is_empty() {
        return Ok(None);
    }
    if args.len() != 2 {
        return Err("ERR syntax error");
    }
    let milliseconds = if args[0].eq_ignore_ascii_case(b"EX") {
        false
    } else if args[0].eq_ignore_ascii_case(b"PX") {
        true
    } else {
        return Err("ERR unsupported SET option");
    };
    let amount = std::str::from_utf8(args[1])
        .ok()
        .filter(|value| !value.is_empty() && value.bytes().all(|c| c.is_ascii_digit()))
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0 && *value <= i64::MAX as u64)
        .ok_or("ERR invalid expire time in 'SET' command")?;
    Ok(Some(if milliseconds {
        Duration::from_millis(amount)
    } else {
        Duration::from_secs(amount)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_layer::CacheLayer;
    use arc_swap::ArcSwap;
    use hyper_util::{client::legacy::Client, rt::TokioExecutor};
    use std::sync::Arc;

    fn state() -> AppState {
        AppState {
            cache: ArcSwap::from(Arc::new(CacheLayer::new(
                "sieve",
                Some("lru"),
                1024,
                Duration::from_secs(60),
                1024,
            ))),
            client: Client::builder(TokioExecutor::new()).build_http(),
            upstream_url: "http://127.0.0.1:1".into(),
        }
    }

    fn command(state: &AppState, args: &[&[u8]]) -> BytesFrame {
        dispatch(
            &BytesFrame::Array(
                args.iter()
                    .map(|arg| BytesFrame::BulkString(Bytes::copy_from_slice(arg)))
                    .collect(),
            ),
            state,
        )
    }

    #[tokio::test]
    async fn binary_keys_and_http_namespace_do_not_collide() {
        let state = state();
        command(&state, &[b"SET", b"\xff", b"first"]);
        command(&state, &[b"SET", b"\xfe", b"second"]);
        assert_eq!(
            command(&state, &[b"GET", b"\xff"]),
            BytesFrame::BulkString("first".into())
        );
        command(&state, &[b"SET", b"GET:/", b"injected"]);
        assert!(state.cache.load().get("GET:/").value.is_none());
        assert_eq!(
            state.cache.load().comparison_stats().unwrap().current_size,
            0
        );
    }

    #[tokio::test]
    async fn malformed_set_does_not_overwrite_existing_value() {
        let state = state();
        command(&state, &[b"SET", b"key", b"old"]);
        for options in [
            vec![b"EX".as_slice()],
            vec![b"EX", b"0"],
            vec![b"EX", b"-1"],
            vec![b"PX", b"no"],
            vec![b"NX"],
            vec![b"EX", b"1", b"NX"],
        ] {
            let mut args = vec![b"SET".as_slice(), b"key", b"new"];
            args.extend(options);
            assert!(matches!(command(&state, &args), BytesFrame::Error(_)));
            assert_eq!(
                command(&state, &[b"GET", b"key"]),
                BytesFrame::BulkString("old".into())
            );
        }
    }

    #[tokio::test]
    async fn persistent_ttl_expiration_and_delete_semantics() {
        let state = state();
        command(&state, &[b"SET", b"key", b"value"]);
        assert_eq!(command(&state, &[b"TTL", b"key"]), BytesFrame::Integer(-1));
        assert_eq!(
            command(&state, &[b"TTL", b"missing"]),
            BytesFrame::Integer(-2)
        );
        state
            .cache
            .load()
            .insert_raw(b"expired", "old".into(), Some(Duration::ZERO));
        assert_eq!(
            command(&state, &[b"DEL", b"expired"]),
            BytesFrame::Integer(0)
        );
        assert_eq!(
            command(&state, &[b"DEL", b"key", b"key"]),
            BytesFrame::Integer(1)
        );
        command(&state, &[b"SET", b"key", b"value", b"PX", b"10000"]);
        assert!(matches!(
            command(&state, &[b"TTL", b"key"]),
            BytesFrame::Integer(0..=10)
        ));
        assert_eq!(state.cache.load().comparison_stats().unwrap().hits, 0);
        assert_eq!(state.cache.load().comparison_stats().unwrap().misses, 0);
    }

    #[tokio::test]
    async fn rejects_wrong_arity_types_and_oversized_values() {
        let state = state();
        assert!(matches!(
            command(&state, &[b"GET", b"a", b"b"]),
            BytesFrame::Error(_)
        ));
        assert!(matches!(
            command(&state, &[b"SET", b"key", &vec![0; 1025]]),
            BytesFrame::Error(_)
        ));
        assert!(matches!(
            dispatch(
                &BytesFrame::Array(vec![
                    BytesFrame::BulkString("SET".into()),
                    BytesFrame::Null,
                    BytesFrame::Null
                ]),
                &state
            ),
            BytesFrame::Error(_)
        ));
        assert_eq!(
            command(&state, &[b"PING", b"hello"]),
            BytesFrame::BulkString("hello".into())
        );
        assert!(matches!(
            command(&state, &[b"EXPIRE", b"key", b"1"]),
            BytesFrame::Error(_)
        ));
    }
}
