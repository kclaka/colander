use crate::cache_layer::{parse_cache_control, CacheLayer};
use arc_swap::ArcSwap;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use bytes::{Bytes, BytesMut};
use futures_util::{stream, StreamExt};
use http_body_util::{BodyExt, BodyStream, StreamBody};
use hyper::body::{Body as _, Frame};
use hyper_util::client::legacy::Client;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{timeout_at, Instant};

pub type HttpClient = Client<hyper_util::client::legacy::connect::HttpConnector, Body>;

pub struct AppState {
    pub cache: ArcSwap<CacheLayer>,
    pub client: HttpClient,
    pub upstream_url: String,
}

/// Timeout is a handler extension so all protocols can share the same AppState.
#[derive(Clone, Copy)]
pub struct UpstreamTimeout(pub Duration);

pub async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(UpstreamTimeout(timeout)): axum::Extension<UpstreamTimeout>,
    req: Request<Body>,
) -> Response<Body> {
    let deadline = Instant::now() + timeout;
    let cache = state.cache.load_full();
    let (mut parts, body) = req.into_parts();
    let invalidates = !matches!(
        parts.method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    );
    let cacheable_request =
        body.is_end_stream() && request_is_cacheable(&parts.method, &parts.headers);
    let cache_key = format!(
        "GET:{}:{}",
        parts
            .headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        parts.uri
    );
    if cacheable_request {
        let lookup = cache.get(&cache_key);
        if let Some(cached) = lookup.value {
            let mut response = Response::builder().status(cached.status);
            for (key, value) in &cached.headers {
                response = response.header(key.as_str(), value.as_str());
            }
            let mut response = response.body(Body::from(cached.body.clone())).unwrap();
            let age = response
                .headers()
                .get("age")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
                .saturating_add(cached.inserted_at.elapsed().as_secs());
            response
                .headers_mut()
                .insert("age", HeaderValue::from_str(&age.to_string()).unwrap());
            add_cache_headers(&mut response, &cache, "HIT");
            return response;
        }
    }

    let upstream_uri = format!(
        "{}{}",
        state.upstream_url.trim_end_matches('/'),
        parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/")
    );
    parts.uri = match upstream_uri.parse() {
        Ok(uri) => uri,
        Err(_) => return gateway_error(StatusCode::BAD_GATEWAY),
    };
    strip_hop_by_hop(&mut parts.headers);
    // Hyper fills Host from the upstream authority instead of the client's host.
    parts.headers.remove("host");
    let upstream = match timeout_at(
        deadline,
        state.client.request(Request::from_parts(parts, body)),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            tracing::warn!(%error, "upstream request failed");
            return gateway_error(StatusCode::BAD_GATEWAY);
        }
        Err(_) => return gateway_error(StatusCode::GATEWAY_TIMEOUT),
    };
    let (mut parts, mut body) = upstream.into_parts();
    if invalidates && parts.status.as_u16() < 400 {
        cache.invalidate_http(&cache_key);
    }
    let cacheable = cacheable_request
        && parts.status == StatusCode::OK
        && response_is_cacheable(&parts.headers);
    let ttl = extract_ttl(&parts.headers).unwrap_or(cache.default_ttl());
    let age = parts
        .headers
        .get("age")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let ttl = ttl.saturating_sub(Duration::from_secs(age));
    strip_hop_by_hop(&mut parts.headers);
    let known_large = parts
        .headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|size| size > cache.max_body_size as u64);

    let mut buffered = BytesMut::new();
    if cacheable && !known_large && !ttl.is_zero() {
        loop {
            match timeout_at(deadline, body.frame()).await {
                Ok(Some(Ok(frame))) => {
                    match frame.into_data() {
                        Ok(data) => buffered.extend_from_slice(&data),
                        Err(trailers) => {
                            // Preserve trailers, but don't store responses that have them.
                            let prefix = stream::iter(vec![
                                Ok(Frame::data(buffered.freeze())),
                                Ok(trailers),
                            ]);
                            let mut response = Response::from_parts(
                                parts,
                                timed_body(prefix.chain(BodyStream::new(body)), deadline),
                            );
                            add_cache_headers(&mut response, &cache, "MISS");
                            return response;
                        }
                    }
                    if buffered.len() > cache.max_body_size {
                        break;
                    }
                }
                Ok(Some(Err(_))) => return gateway_error(StatusCode::BAD_GATEWAY),
                Err(_) => return gateway_error(StatusCode::GATEWAY_TIMEOUT),
                Ok(None) => {
                    let bytes = buffered.freeze();
                    let headers = parts
                        .headers
                        .iter()
                        .map(|(key, value)| (key.to_string(), value.to_str().unwrap().to_string()))
                        .collect();
                    cache.insert(
                        cache_key,
                        cache.build_response(
                            parts.status.as_u16(),
                            headers,
                            bytes.clone(),
                            Some(ttl),
                        ),
                    );
                    let mut response = Response::from_parts(parts, Body::from(bytes));
                    add_cache_headers(&mut response, &cache, "MISS");
                    return response;
                }
            }
        }
    }
    // Non-cacheable and oversized responses are streamed with bounded buffering.
    let prefix = stream::iter(if buffered.is_empty() {
        vec![]
    } else {
        vec![Ok(Frame::data(buffered.freeze()))]
    });
    let mut response = Response::from_parts(
        parts,
        timed_body(prefix.chain(BodyStream::new(body)), deadline),
    );
    add_cache_headers(&mut response, &cache, "MISS");
    response
}

fn timed_body<S>(frames: S, deadline: Instant) -> Body
where
    S: futures_util::Stream<Item = Result<Frame<Bytes>, hyper::Error>> + Send + 'static,
{
    let frames = Box::pin(frames);
    Body::new(StreamBody::new(stream::unfold(
        Some(frames),
        move |state| async move {
            let mut frames = state?;
            match timeout_at(deadline, frames.next()).await {
                Ok(Some(Ok(frame))) => Some((Ok(frame), Some(frames))),
                Ok(Some(Err(error))) => Some((Err(std::io::Error::other(error)), None)),
                Ok(None) => None,
                Err(_) => Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "upstream body timed out",
                    )),
                    None,
                )),
            }
        },
    )))
}

fn add_cache_headers(response: &mut Response<Body>, cache: &CacheLayer, status: &'static str) {
    let headers = response.headers_mut();
    headers.insert("x-cache", HeaderValue::from_static(status));
    headers.insert(
        "x-cache-policy",
        HeaderValue::from_static(cache.primary_name()),
    );
    headers.insert(
        "x-mode",
        HeaderValue::from_static(if cache.is_demo_mode() {
            "demo"
        } else {
            "bench"
        }),
    );
}

fn gateway_error(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::from(
            status.canonical_reason().unwrap_or("Upstream error"),
        ))
        .unwrap()
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let nominated: Vec<String> = headers
        .get_all("connection")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(',').map(|name| name.trim().to_owned()))
        .collect();
    for name in nominated {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "proxy-connection",
    ] {
        headers.remove(name);
    }
}

fn request_is_cacheable(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::GET
        && headers
            .get("content-length")
            .is_none_or(|value| value == "0")
        && !headers.keys().any(|name| {
            matches!(
                name.as_str(),
                "authorization"
                    | "cookie"
                    | "range"
                    | "cache-control"
                    | "pragma"
                    | "transfer-encoding"
            ) || name.as_str().starts_with("if-")
        })
}

fn response_is_cacheable(headers: &HeaderMap) -> bool {
    // Until variant selection is implemented, any Vary response bypasses storage.
    !headers.contains_key("set-cookie")
        && !headers.contains_key("vary")
        && !headers.contains_key("content-range")
        && headers.values().all(|value| value.to_str().is_ok())
        && parse_cache_control(&cache_control(headers)).cacheable
}

fn cache_control(headers: &HeaderMap) -> String {
    headers
        .get_all("cache-control")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>()
        .join(",")
}

fn extract_ttl(headers: &HeaderMap) -> Option<Duration> {
    parse_cache_control(&cache_control(headers)).max_age
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::any, Router};
    use hyper_util::rt::TokioExecutor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn origin(req: Request<Body>) -> Response<Body> {
        let path = req.uri().path().to_owned();
        if path == "/slow" {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if path == "/echo" {
            assert!(!req.headers().contains_key("x-hop"));
            assert_ne!(req.headers().get("host").unwrap(), "client.example");
            let auth = req.headers().get("authorization").unwrap().clone();
            let content_type = req.headers().get("content-type").unwrap().clone();
            let bytes = req.into_body().collect().await.unwrap().to_bytes();
            return Response::builder()
                .header("x-auth", auth)
                .header("content-type", content_type)
                .body(Body::from(bytes))
                .unwrap();
        }
        let mut response = Response::builder().header("cache-control", "public, max-age=60");
        match path.as_str() {
            "/private" => response = response.header("set-cookie", "session=secret"),
            "/vary" => response = response.header("vary", "Accept-Language"),
            "/multiple" => response = response.header("cache-control", "private=\"X-Secret\""),
            "/age" => response = response.header("age", "120"),
            "/hop" => {
                response = response
                    .header("connection", "x-hop")
                    .header("x-hop", "remove me")
            }
            "/body-timeout" | "/stream-timeout" => {
                if path == "/stream-timeout" {
                    response = response.header("cache-control", "no-store");
                }
                return response
                    .body(Body::from_stream(stream::once(async {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        Ok::<_, std::io::Error>(Bytes::from_static(b"body"))
                    })))
                    .unwrap();
            }
            "/large" => {
                return response
                    .body(Body::from_stream(stream::iter([
                        Ok::<_, std::io::Error>(Bytes::from_static(b"1234")),
                        Ok(Bytes::from_static(b"5678")),
                        Ok(Bytes::from_static(b"90")),
                    ])))
                    .unwrap()
            }
            _ => {}
        }
        response.body(Body::from("body")).unwrap()
    }

    async fn setup() -> (Arc<AppState>, tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let router = Router::new().fallback(any(move |request: Request<Body>| {
            observed.fetch_add(1, Ordering::Relaxed);
            origin(request)
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let state = Arc::new(AppState {
            cache: ArcSwap::from(Arc::new(CacheLayer::new(
                "sieve",
                Some("lru"),
                1024,
                Duration::from_secs(60),
                4,
            ))),
            client: Client::builder(TokioExecutor::new()).build_http(),
            upstream_url: format!("http://{address}"),
        });
        (state, server, calls)
    }

    async fn request(state: &Arc<AppState>, req: Request<Body>) -> Response<Body> {
        proxy_handler(
            State(state.clone()),
            axum::Extension(UpstreamTimeout(Duration::from_secs(2))),
            req,
        )
        .await
    }

    fn get(path: &str) -> Request<Body> {
        Request::builder().uri(path).body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn forwards_headers_and_body_without_hop_headers() {
        let (state, server, _) = setup().await;
        let response = request(
            &state,
            Request::builder()
                .method("POST")
                .uri("/echo")
                .header("host", "client.example")
                .header("authorization", "Bearer token")
                .header("content-type", "application/json")
                .header("connection", "x-hop")
                .header("x-hop", "secret")
                .body(Body::from("{\"ok\":true}"))
                .unwrap(),
        )
        .await;
        assert_eq!(response.headers()["x-auth"], "Bearer token");
        assert_eq!(response.headers()["content-type"], "application/json");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "{\"ok\":true}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn cache_hits_preserve_body_but_never_replay_hop_headers() {
        let (state, server, calls) = setup().await;
        for expected in ["MISS", "HIT"] {
            let response = request(&state, get("/hop")).await;
            assert_eq!(response.headers()["x-cache"], expected);
            assert!(!response.headers().contains_key("x-hop"));
            assert!(!response.headers().contains_key("connection"));
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                "body"
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        server.abort();
    }

    #[tokio::test]
    async fn personalized_conditional_and_cache_directive_requests_bypass_hits() {
        let (state, server, _) = setup().await;
        request(&state, get("/public")).await;
        for (header, value) in [
            ("authorization", "Bearer secret"),
            ("cookie", "session=1"),
            ("range", "bytes=0-1"),
            ("if-none-match", "etag"),
            ("cache-control", "no-cache"),
            ("pragma", "no-cache"),
        ] {
            let req = Request::builder()
                .uri("/public")
                .header(header, value)
                .body(Body::empty())
                .unwrap();
            assert_eq!(request(&state, req).await.headers()["x-cache"], "MISS");
        }
        for path in ["/private", "/vary", "/multiple", "/age"] {
            for _ in 0..2 {
                assert_eq!(
                    request(&state, get(path)).await.headers()["x-cache"],
                    "MISS",
                    "{path}"
                );
            }
        }
        server.abort();
    }

    #[tokio::test]
    async fn successful_mutations_invalidate_cached_get_responses() {
        let (state, server, _) = setup().await;
        request(&state, get("/resource")).await;
        assert_eq!(
            request(&state, get("/resource")).await.headers()["x-cache"],
            "HIT"
        );
        request(
            &state,
            Request::builder()
                .method("PUT")
                .uri("/resource")
                .body(Body::from("new"))
                .unwrap(),
        )
        .await;
        assert_eq!(
            request(&state, get("/resource")).await.headers()["x-cache"],
            "MISS"
        );
        server.abort();
    }

    #[tokio::test]
    async fn streams_oversized_chunked_responses_without_truncation_or_caching() {
        let (state, server, _) = setup().await;
        for _ in 0..2 {
            let response = request(&state, get("/large")).await;
            assert_eq!(response.headers()["x-cache"], "MISS");
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                "1234567890"
            );
        }
        assert_eq!(state.cache.load().primary_stats().current_size, 0);
        server.abort();
    }

    #[tokio::test]
    async fn timeouts_cover_headers_buffered_bodies_and_streamed_bodies() {
        let (state, server, _) = setup().await;
        for path in ["/slow", "/body-timeout", "/stream-timeout"] {
            let response = proxy_handler(
                State(state.clone()),
                axum::Extension(UpstreamTimeout(Duration::from_millis(20))),
                get(path),
            )
            .await;
            if path == "/stream-timeout" {
                assert!(response.into_body().collect().await.is_err());
            } else {
                assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
            }
        }
        server.abort();
    }

    #[test]
    fn shared_freshness_precedence_and_invalid_directives() {
        for value in ["s-maxage=5, max-age=60", "max-age=60, S-Maxage=\"5\""] {
            assert_eq!(
                parse_cache_control(value).max_age,
                Some(Duration::from_secs(5))
            );
        }
        for value in [
            "private=\"foo\"",
            "no-cache=\"foo\"",
            "max-age=oops",
            "max-age=1, max-age=2",
        ] {
            assert!(!parse_cache_control(value).cacheable, "{value}");
        }
    }
}
