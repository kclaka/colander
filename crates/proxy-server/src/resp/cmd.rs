use crate::proxy::AppState;
use bytes::Bytes;
use redis_protocol::resp2::types::BytesFrame;
use std::time::Duration;

/// Dispatch a RESP2 frame (expected to be an Array of bulk strings) to the appropriate handler.
pub fn dispatch(frame: &BytesFrame, state: &AppState) -> BytesFrame {
    let args = match frame {
        BytesFrame::Array(arr) => arr,
        _ => return error_frame("ERR expected array"),
    };

    if args.is_empty() {
        return error_frame("ERR empty command");
    }

    let cmd = match &args[0] {
        BytesFrame::BulkString(b) => String::from_utf8_lossy(b).to_uppercase(),
        _ => return error_frame("ERR invalid command format"),
    };

    let cache = state.cache.load();

    match cmd.as_str() {
        "PING" => BytesFrame::SimpleString("PONG".into()),
        "COMMAND" => BytesFrame::SimpleString("OK".into()),
        "GET" => {
            if args.len() < 2 {
                return error_frame("ERR wrong number of arguments for 'GET' command");
            }
            let key = bulk_to_string(&args[1]);
            let lookup = cache.get(&key);
            match lookup.value {
                Some(cached) => BytesFrame::BulkString(cached.body.clone()),
                None => BytesFrame::Null,
            }
        }
        "SET" => {
            if args.len() < 3 {
                return error_frame("ERR wrong number of arguments for 'SET' command");
            }
            let key = bulk_to_string(&args[1]);
            let value = bulk_to_bytes(&args[2]);
            let ttl = parse_set_options(&args[3..]);
            cache.insert_raw(key, value, ttl);
            BytesFrame::SimpleString("OK".into())
        }
        "DEL" => {
            if args.len() < 2 {
                return error_frame("ERR wrong number of arguments for 'DEL' command");
            }
            let mut count: i64 = 0;
            for arg in &args[1..] {
                let key = bulk_to_string(arg);
                if cache.remove(&key) {
                    count += 1;
                }
            }
            BytesFrame::Integer(count)
        }
        "EXPIRE" => {
            // TTL is set-at-insert only — EXPIRE is not supported
            BytesFrame::Integer(0)
        }
        "TTL" => {
            if args.len() < 2 {
                return error_frame("ERR wrong number of arguments for 'TTL' command");
            }
            let key = bulk_to_string(&args[1]);
            match cache.ttl_remaining(&key) {
                Some(remaining) => BytesFrame::Integer(remaining.as_secs() as i64),
                None => BytesFrame::Integer(-2),
            }
        }
        other => error_frame(&format!("ERR unknown command '{other}'")),
    }
}

fn error_frame(msg: &str) -> BytesFrame {
    BytesFrame::Error(msg.into())
}

fn bulk_to_string(frame: &BytesFrame) -> String {
    match frame {
        BytesFrame::BulkString(b) => String::from_utf8_lossy(b).into_owned(),
        _ => String::new(),
    }
}

fn bulk_to_bytes(frame: &BytesFrame) -> Bytes {
    match frame {
        BytesFrame::BulkString(b) => b.clone(),
        _ => Bytes::new(),
    }
}

/// Parse SET options: SET key value [EX seconds]
fn parse_set_options(args: &[BytesFrame]) -> Option<Duration> {
    let mut i = 0;
    while i < args.len() {
        let opt = bulk_to_string(&args[i]).to_uppercase();
        if opt == "EX" && i + 1 < args.len() {
            let secs_str = bulk_to_string(&args[i + 1]);
            if let Ok(secs) = secs_str.parse::<u64>() {
                return Some(Duration::from_secs(secs));
            }
        }
        i += 1;
    }
    None
}
