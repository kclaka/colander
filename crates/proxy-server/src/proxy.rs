use crate::cache_layer::{parse_cache_control, CacheLayer};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Request, Response, StatusCode};
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use std::sync::Arc;
use std::time::Instant;

pub type HttpClient = Client<
    hyper_util::client::legacy::connect::HttpConnector,
    Body,
>;

/// Shared application state passed to all handlers.
pub struct AppState {
    pub cache: CacheLayer,
    pub client: HttpClient,
    pub upstream_url: String,
}

/// Main proxy handler. Checks cache, forwards to upstream on miss, caches response.
pub async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response<Body> {
    let start = Instant::now();
    let method = req.method().clone();
    let uri = req.uri().clone();

    // Only cache GET requests
    let cacheable_method = method == axum::http::Method::GET;

    let cache_key = format!("{}:{}", method, uri);

    // Check cache for GET requests
    if cacheable_method {
        let lookup = state.cache.get(&cache_key);
        if lookup.is_hit() {
            let cached = lookup.value.unwrap();
            let elapsed = start.elapsed();

            tracing::debug!(
                key = %cache_key,
                latency_us = elapsed.as_micros(),
                "cache HIT"
            );

            return build_cached_response(&cached, &state, true);
        }
    }

    // Cache miss — forward to upstream
    let upstream_uri = format!(
        "{}{}",
        state.upstream_url.trim_end_matches('/'),
        uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/")
    );

    let upstream_req = match Request::builder()
        .method(&method)
        .uri(&upstream_uri)
        .body(req.into_body())
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "failed to build upstream request");
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("Bad Gateway"))
                .unwrap();
        }
    };

    let upstream_resp = match state.client.request(upstream_req).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(error = %e, upstream = %upstream_uri, "upstream request failed");
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("Bad Gateway"))
                .unwrap();
        }
    };

    let status = upstream_resp.status();
    let headers = upstream_resp.headers().clone();

    // Read the full response body
    let body_bytes = match upstream_resp.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            tracing::error!(error = %e, "failed to read upstream response body");
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("Bad Gateway"))
                .unwrap();
        }
    };

    // Determine if we should cache this response
    let should_cache = cacheable_method
        && status == StatusCode::OK
        && body_bytes.len() <= state.cache.max_body_size
        && is_cacheable_headers(&headers);

    let ttl = extract_ttl(&headers);

    if should_cache {
        let response_headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        let cached_response =
            state
                .cache
                .build_response(status.as_u16(), response_headers, body_bytes.clone(), ttl);

        state.cache.insert(cache_key.clone(), cached_response);
    }

    let elapsed = start.elapsed();
    tracing::debug!(
        key = %cache_key,
        status = status.as_u16(),
        cached = should_cache,
        latency_us = elapsed.as_micros(),
        "cache MISS → upstream"
    );

    // Build response from upstream
    let mut response = Response::builder().status(status);

    // Copy upstream headers
    for (key, value) in headers.iter() {
        // Skip hop-by-hop headers
        let k = key.as_str();
        if k == "transfer-encoding" || k == "connection" {
            continue;
        }
        response = response.header(key, value);
    }

    // Add cache status headers
    response = response
        .header("X-Cache", "MISS")
        .header("X-Cache-Policy", state.cache.primary_name())
        .header(
            "X-Mode",
            if state.cache.is_demo_mode() {
                "demo"
            } else {
                "bench"
            },
        );

    response.body(Body::from(body_bytes)).unwrap()
}

/// Build an HTTP response from a cached entry.
fn build_cached_response(
    cached: &colander_cache::traits::CachedResponse,
    state: &AppState,
    _hit: bool,
) -> Response<Body> {
    let mut response = Response::builder().status(cached.status);

    for (key, value) in &cached.headers {
        if let Ok(v) = HeaderValue::from_str(value) {
            response = response.header(key.as_str(), v);
        }
    }

    response = response
        .header("X-Cache", "HIT")
        .header("X-Cache-Policy", state.cache.primary_name())
        .header(
            "X-Mode",
            if state.cache.is_demo_mode() {
                "demo"
            } else {
                "bench"
            },
        );

    response.body(Body::from(cached.body.clone())).unwrap()
}

/// Check if response headers allow caching.
fn is_cacheable_headers(headers: &HeaderMap) -> bool {
    if let Some(cc) = headers.get("cache-control") {
        if let Ok(cc_str) = cc.to_str() {
            return parse_cache_control(cc_str).cacheable;
        }
    }
    // No Cache-Control header — cacheable by default
    true
}

/// Extract TTL from Cache-Control header.
fn extract_ttl(headers: &HeaderMap) -> Option<std::time::Duration> {
    if let Some(cc) = headers.get("cache-control") {
        if let Ok(cc_str) = cc.to_str() {
            return parse_cache_control(cc_str).max_age;
        }
    }
    None
}
