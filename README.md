<p align="center">
  <strong>Colander</strong><br>
  High-performance HTTP caching reverse proxy powered by SIEVE
</p>

<p align="center">
  <a href="https://github.com/kclaka/colander/actions/workflows/ci.yml"><img src="https://github.com/kclaka/colander/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License: MIT"></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/rust-2021_edition-orange.svg" alt="Rust"></a>
</p>

---

Colander is a drop-in caching reverse proxy that replaces LRU with the [SIEVE](https://cachemon.github.io/SIEVE-website/) eviction algorithm (published at [NSDI '24](https://www.usenix.org/conference/nsdi24/presentation/zhang-yazhuo)). It sits between your clients and your backend, caches HTTP responses, and speaks both **HTTP** and the **Redis wire protocol (RESP2)** — so you can swap out Redis for SIEVE-powered caching with zero code changes.

```bash
docker compose up          # proxy + backend + load generator + dashboard
open http://localhost:3001  # watch SIEVE outperform LRU in real time
```

---

## Table of Contents

- [Why SIEVE](#why-sieve)
- [Architecture](#architecture)
- [Features](#features)
- [Quick Start](#quick-start)
  - [Docker (recommended)](#docker-recommended)
  - [From source](#from-source)
- [Configuration](#configuration)
  - [Server](#server)
  - [Upstream](#upstream)
  - [Cache](#cache)
  - [RESP](#resp)
  - [Hot-Reload](#hot-reload)
- [Redis Interface (RESP2)](#redis-interface-resp2)
- [Prometheus Metrics](#prometheus-metrics)
- [Live Dashboard](#live-dashboard)
- [HTTP Response Headers](#http-response-headers)
- [Admin API](#admin-api)
- [Cache Design](#cache-design)
  - [Eviction Policies](#eviction-policies)
  - [Arena Allocation](#arena-allocation)
  - [64-Shard Concurrency](#64-shard-concurrency)
  - [Lazy TTL Expiration](#lazy-ttl-expiration)
- [Project Structure](#project-structure)
- [Development](#development)
- [References](#references)
- [License](#license)

---

## Why SIEVE

Traditional caches use **LRU**, which moves every accessed item to the front of a linked list. That means a **write lock on every cache hit** — a scalability wall on multi-core systems.

**SIEVE** replaces move-to-front with a single atomic bit flip on hit. A roving "hand" pointer handles eviction by scanning from tail to head, keeping visited items in place and evicting cold ones. No list mutation. No write contention.

| Property | LRU | SIEVE |
|----------|-----|-------|
| Hit operation | Move-to-front (write lock) | Flip visited bit (lock-free `AtomicBool`) |
| Eviction | Always evict tail | Hand scans for unvisited |
| Miss ratio | Baseline | [Up to 63% lower](https://www.usenix.org/conference/nsdi24/presentation/zhang-yazhuo) than ARC |
| Multi-thread scaling | Limited by write contention | Near-linear to 16+ threads |
| Parameters | None | None |
| Per-object metadata | Pointer × 2 | 1 bit (visited) + pointer × 2 |

> **Key insight from the paper**: SIEVE achieves "lazy promotion" and "quick demotion" — popular objects stay in place without being moved, while one-hit wonders are quickly evicted. This makes it especially effective for web cache workloads with power-law (Zipfian) access patterns.

Read the full paper: [*SIEVE is Simpler than LRU: an Efficient Turn-Key Eviction Algorithm for Web Caches*](https://www.usenix.org/conference/nsdi24/presentation/zhang-yazhuo) (NSDI '24).

---

## Architecture

```
                    ┌──────────────────────────────────────────────┐
                    │              Colander Proxy                  │
                    │                                              │
┌──────────┐  HTTP  │  ┌──────────────────────────────────────┐   │        ┌──────────┐
│  Clients │───:8080──▶│           Cache Layer                │   │──:3000─▶│ Backend  │
└──────────┘       │  │  ┌──────────┐       ┌──────────┐     │   │        └──────────┘
                    │  │  │  SIEVE   │       │   LRU    │     │   │
┌──────────┐  RESP  │  │  │ (primary)│       │ (shadow) │     │   │
│ Redis    │───:6379──▶│  └──────────┘       └──────────┘     │   │
│ clients  │       │  │       64 shards × RwLock              │   │
└──────────┘       │  └──────────────────────────────────────┘   │
                    │                                              │
┌──────────┐  WS    │           Metrics Engine                    │
│Dashboard │◀──:9090──│  WebSocket broadcast @ 500ms              │
│ (React)  │       │  │  Prometheus GET /metrics                  │
└──────────┘       │  │  Admin API (mode toggle, stats)           │
                    └──────────────────────────────────────────────┘

┌──────────┐
│ Load Gen │  Zipfian(α) traffic → proxy:8080
│ (tunable)│  POST /control to adjust α at runtime
└──────────┘
```

**Dual-cache mode**: every request hits both SIEVE (primary) and LRU (comparison). Responses are served from SIEVE; LRU runs in shadow mode for a fair, same-traffic comparison. Toggle to **bench mode** via the [Admin API](#admin-api) for single-policy throughput numbers.

---

## Features

| Category | Feature |
|----------|---------|
| **Caching** | SIEVE, LRU, and FIFO eviction policies behind a common trait |
| **Protocols** | HTTP reverse proxy (`:8080`) + [RESP2 Redis interface](#redis-interface-resp2) (`:6379`) |
| **Observability** | [Prometheus metrics](#prometheus-metrics) (`:9090/metrics`), WebSocket live stream, [React dashboard](#live-dashboard) |
| **Operability** | [Graceful shutdown](#graceful-shutdown) (SIGINT/SIGTERM), [config hot-reload](#hot-reload), per-policy stats |
| **Performance** | 64-shard concurrency, arena-allocated linked lists, lock-free hits (SIEVE), `ahash` for DoS-resistant sharding |
| **DevOps** | Docker Compose one-click demo, [GitHub Actions CI](#development) (fmt + clippy + test) |

---

## Quick Start

### Docker (recommended)

```bash
docker compose up --build
```

| Service | Port | Description |
|---------|------|-------------|
| Proxy | [`localhost:8080`](http://localhost:8080) | HTTP caching reverse proxy |
| Metrics | [`localhost:9090`](http://localhost:9090) | Prometheus + WebSocket + Admin API |
| RESP | `localhost:6379` | Redis wire protocol interface |
| Dashboard | [`localhost:3001`](http://localhost:3001) | Live SIEVE vs LRU charts |
| Backend | `localhost:3000` | Demo origin with 5–20ms latency |
| Load Gen | `localhost:9091` | Zipfian traffic control |

### From source

Prerequisites: [Rust](https://www.rust-lang.org/tools/install) (1.70+), [Node.js](https://nodejs.org/) 18+ (for dashboard only).

```bash
# Terminal 1 — demo backend
cargo run -p demo-backend

# Terminal 2 — proxy
cargo run -p proxy-server

# Terminal 3 — test it
curl -v http://localhost:8080/api/items/42   # X-Cache: MISS
curl -v http://localhost:8080/api/items/42   # X-Cache: HIT

# Redis protocol
redis-cli -p 6379 PING                      # PONG
redis-cli -p 6379 SET foo bar               # OK
redis-cli -p 6379 GET foo                   # "bar"
```

---

## Configuration

Colander reads from `config.toml` in the working directory. All fields have defaults — the file is optional.

### Server

```toml
[server]
listen_addr = "0.0.0.0:8080"    # HTTP proxy bind address
metrics_addr = "0.0.0.0:9090"   # Metrics/admin bind address
```

### Upstream

```toml
[upstream]
url = "http://localhost:3000"    # Backend origin URL
timeout_ms = 5000                # Upstream request timeout
```

### Cache

```toml
[cache]
capacity = 10000                 # Max entries across all shards
default_ttl_seconds = 60         # Default TTL when Cache-Control is absent
max_body_size_bytes = 1048576    # 1 MB — responses larger than this are not cached
eviction_policy = "sieve"        # Primary policy: "sieve", "lru", or "fifo"
comparison_policy = "lru"        # Shadow policy for dual-cache comparison (optional)
```

### RESP

```toml
[resp]
enabled = true                   # Enable/disable Redis protocol interface
listen_addr = "0.0.0.0:6379"    # RESP bind address
```

### Hot-Reload

Colander watches `config.toml` for changes at runtime. When a change is detected:

| Field | Behavior | Downtime |
|-------|----------|----------|
| `default_ttl_seconds` | Applied immediately via atomic swap | **None** — cache data preserved |
| `eviction_policy` / `comparison_policy` | Cache rebuilt with new policy | Cache cleared (cold start) |
| `capacity` | **Ignored** — logged as WARN | Restart required |

> **Why capacity changes are rejected**: If a running cache is full (e.g., 1M items) and capacity drops to 500K, the next request would synchronously evict 500K items in a tight loop, stalling the event loop and spiking P99 latency. Colander prioritizes stability over flexibility — restart to resize safely.

---

## Redis Interface (RESP2)

Colander speaks the [Redis Serialization Protocol](https://redis.io/docs/latest/develop/reference/protocol-spec/) on port `6379`. Point any Redis client at Colander and get SIEVE-powered caching — no code changes needed.

```bash
redis-cli -p 6379
```

### Supported Commands

| Command | Syntax | Description |
|---------|--------|-------------|
| **PING** | `PING` | Health check. Returns `PONG`. |
| **GET** | `GET key` | Retrieve a cached value. Returns bulk string or `(nil)`. |
| **SET** | `SET key value [EX seconds]` | Store a value with optional TTL. Returns `OK`. |
| **DEL** | `DEL key [key ...]` | Delete one or more keys. Returns count of deleted keys. |
| **TTL** | `TTL key` | Seconds remaining before expiry. Returns `-2` if key missing. |
| **EXPIRE** | `EXPIRE key seconds` | Not supported (TTL is set-at-insert). Returns `0`. |
| **COMMAND** | `COMMAND` | Client compatibility (redis-cli sends this on connect). Returns `OK`. |

### Example

```bash
$ redis-cli -p 6379
127.0.0.1:6379> SET session:abc '{"user":"kenny"}' EX 300
OK
127.0.0.1:6379> GET session:abc
"{\"user\":\"kenny\"}"
127.0.0.1:6379> TTL session:abc
(integer) 299
127.0.0.1:6379> DEL session:abc
(integer) 1
127.0.0.1:6379> GET session:abc
(nil)
```

> **Shared cache**: The RESP interface shares the same in-memory cache as the HTTP proxy. A `SET` via Redis is visible to HTTP `GET` responses, and vice versa.

---

## Prometheus Metrics

Colander exposes [Prometheus](https://prometheus.io/)-compatible metrics at `GET :9090/metrics`.

```bash
curl http://localhost:9090/metrics
```

### Available Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `colander_cache_hits_total` | counter | `policy` | Total cache hits |
| `colander_cache_misses_total` | counter | `policy` | Total cache misses |
| `colander_cache_keys` | gauge | `policy` | Current number of cached entries |
| `colander_cache_evictions_total` | gauge | `policy` | Total evictions |
| `colander_request_duration_seconds` | histogram | — | End-to-end request latency |
| `colander_upstream_duration_seconds` | histogram | — | Upstream (origin) latency on cache misses |

### Grafana

Add `http://proxy:9090` as a Prometheus data source in [Grafana](https://grafana.com/) and import/build dashboards using the metrics above.

---

## Live Dashboard

The React dashboard at [`localhost:3001`](http://localhost:3001) connects via WebSocket to the proxy's metrics engine and renders:

- **Hit rate chart** — SIEVE vs LRU hit rate over time
- **Throughput chart** — requests/second over time
- **Stats cards** — live counters for hits, misses, evictions, cache size, uptime
- **Alpha slider** — adjust the Zipfian skewness parameter (α) of the load generator in real time
- **Mode toggle** — switch between Demo (dual-cache) and Bench (single-cache) mode

Built with [Vite](https://vitejs.dev/), [React](https://react.dev/), and [Recharts](https://recharts.org/).

---

## HTTP Response Headers

Colander adds the following headers to every proxied response:

| Header | Values | Description |
|--------|--------|-------------|
| `X-Cache` | `HIT` / `MISS` | Whether the response was served from cache |
| `X-Cache-Policy` | `SIEVE` / `LRU` / `FIFO` | Which eviction policy served the response |
| `X-Mode` | `demo` / `bench` | Current cache mode |

### Caching Behavior

- Only **GET** requests with **200 OK** responses are cached
- Responses larger than `max_body_size_bytes` are not cached
- `Cache-Control: no-store`, `no-cache`, and `private` are respected
- `s-maxage` takes precedence over `max-age` (as per [RFC 9111](https://www.rfc-editor.org/rfc/rfc9111))

---

## Admin API

The metrics port (`:9090`) exposes administrative endpoints:

### `GET /api/stats`

Returns a JSON snapshot of current cache statistics.

```bash
curl http://localhost:9090/api/stats
```

```json
{
  "primary": { "name": "SIEVE", "hit_rate": 0.72, "hits": 14400, "misses": 5600, "evictions": 3200, "size": 9800, "capacity": 10000 },
  "comparison": { "name": "LRU", "hit_rate": 0.65, "hits": 13000, "misses": 7000, "evictions": 4100, "size": 9800, "capacity": 10000 },
  "mode": "demo"
}
```

### `POST /api/mode`

Toggle between demo (dual-cache) and bench (single-cache) mode.

```bash
curl -X POST http://localhost:9090/api/mode \
  -H 'Content-Type: application/json' \
  -d '{"mode": "bench"}'
```

### `GET /ws/metrics`

WebSocket endpoint streaming [`MetricsSnapshot`](crates/proxy-server/src/metrics.rs) JSON every 500ms. Used by the [dashboard](#live-dashboard).

### `GET /metrics`

[Prometheus text format](#prometheus-metrics) metrics endpoint.

---

## Cache Design

### Eviction Policies

The [`colander-cache`](crates/colander-cache/) crate implements three eviction policies behind a common [`CachePolicy`](crates/colander-cache/src/traits.rs) trait:

| Policy | Hit Behavior | Eviction | Best For |
|--------|-------------|----------|----------|
| **SIEVE** | Flip visited bit (`AtomicBool`) — no list mutation | Hand scans tail→head, evicts unvisited | Web caches, Zipfian workloads |
| **LRU** | Move-to-front (requires write lock) | Evict tail (least recently used) | General purpose, baseline comparison |
| **FIFO** | No-op (no promotion) | Evict tail (oldest) | Scan-heavy workloads |

### Arena Allocation

All policies use an **arena-allocated doubly-linked list** ([`arena.rs`](crates/colander-cache/src/arena.rs)):

- Nodes stored in a `Vec<Option<Node>>` with `u32` indices instead of raw pointers
- Free-list tracks reclaimed slots for O(1) allocation
- Zero `unsafe` code — the borrow checker is satisfied through index-based access
- Cache-line friendly due to contiguous memory layout

### 64-Shard Concurrency

[`ShardedCache<T>`](crates/colander-cache/src/sharded.rs) distributes keys across **64 independent shards** via [`ahash`](https://crates.io/crates/ahash):

- Each shard has its own `parking_lot::RwLock`, arena, and eviction state
- On a cache hit, only **1 of 64 shards** is locked
- Shard selection: `ahash(key) & 0x3F` (bitmask for constant-time modulo)
- SIEVE hits need only a read lock (the visited bit is `AtomicBool`)

### Lazy TTL Expiration

Colander uses **lazy expiration** — expired entries are not proactively garbage-collected:

- On `get()`: if the entry's TTL has elapsed, it's treated as a miss and removed
- On eviction sweep: the SIEVE hand evicts expired entries regardless of their visited bit
- This avoids background timer threads and keeps the hot path fast

---

## Project Structure

```
colander/
├── crates/
│   ├── colander-cache/        # Cache library: SIEVE, LRU, FIFO, arena, sharded wrapper
│   │   ├── src/
│   │   │   ├── traits.rs      # CachePolicy trait, CachedResponse, CacheStats
│   │   │   ├── sieve.rs       # SIEVE implementation
│   │   │   ├── lru.rs         # LRU implementation
│   │   │   ├── fifo.rs        # FIFO implementation
│   │   │   ├── arena.rs       # Arena-allocated doubly-linked list
│   │   │   └── sharded.rs     # 64-shard concurrent wrapper
│   │   └── benches/
│   │       └── cache_bench.rs # Criterion benchmarks
│   ├── proxy-server/          # HTTP reverse proxy + RESP server + metrics
│   │   └── src/
│   │       ├── main.rs        # Entry point, server setup, config watcher
│   │       ├── proxy.rs       # Axum proxy handler, upstream forwarding
│   │       ├── cache_layer.rs # Dual-cache wrapper, mode toggle, raw insert
│   │       ├── config.rs      # TOML config parsing, hot-reload diff
│   │       ├── metrics.rs     # WebSocket broadcast, stats/mode endpoints
│   │       └── resp/          # RESP2 Redis protocol server
│   │           ├── mod.rs     # TCP listener, connection accept loop
│   │           ├── connection.rs  # Per-connection frame codec
│   │           └── cmd.rs     # Command dispatch (GET, SET, DEL, TTL, PING)
│   ├── loadgen/               # Zipfian traffic generator with adjustable α
│   └── demo-backend/          # Fake origin API with 5–20ms artificial latency
├── dashboard/                 # React + Vite + Recharts live metrics UI
├── docker/                    # Dockerfiles for Rust binaries and dashboard
├── .github/workflows/ci.yml  # GitHub Actions: fmt, clippy, test
├── config.toml                # Local development configuration
└── docker-compose.yml         # One-click demo orchestration
```

---

## Development

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) 1.70+
- [Node.js](https://nodejs.org/) 18+ (dashboard only)
- [Docker](https://www.docker.com/) (for compose demo)

### Commands

```bash
cargo build --workspace         # Build all crates
cargo test --workspace          # Run all 48 tests
cargo test -p colander-cache    # Cache library tests only
cargo bench -p colander-cache   # SIEVE vs LRU throughput benchmarks
cargo clippy --workspace        # Lint check
cargo fmt --all                 # Format code
```

### CI

Every push and pull request runs [GitHub Actions](.github/workflows/ci.yml):

1. `cargo fmt --all -- --check` — formatting
2. `cargo clippy --workspace -- -D warnings` — lints (warnings are errors)
3. `cargo test --workspace` — all tests

### Graceful Shutdown

On `SIGINT` (Ctrl+C) or `SIGTERM`:

1. Stop accepting new connections on all servers (HTTP, metrics, RESP)
2. Drain in-flight requests to completion
3. Exit cleanly

This ensures zero dropped requests during rolling deployments.

---

## References

- [SIEVE is Simpler than LRU: an Efficient Turn-Key Eviction Algorithm for Web Caches](https://www.usenix.org/conference/nsdi24/presentation/zhang-yazhuo) — Yazhuo Zhang et al., NSDI '24
- [SIEVE Project Page](https://cachemon.github.io/SIEVE-website/) — interactive visualizations and trace results
- [Redis Serialization Protocol (RESP)](https://redis.io/docs/latest/develop/reference/protocol-spec/) — wire protocol specification
- [Prometheus Exposition Formats](https://prometheus.io/docs/instrumenting/exposition_formats/) — metrics text format
- [RFC 9111 — HTTP Caching](https://www.rfc-editor.org/rfc/rfc9111) — `Cache-Control` semantics

---

## License

[MIT](LICENSE) &copy; 2026 KennyIgbechi
