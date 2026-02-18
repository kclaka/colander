# Colander

A high-performance HTTP caching reverse proxy powered by the [SIEVE](https://cachemon.github.io/SIEVE-website/) eviction algorithm.

SIEVE (NSDI '24) is simpler than LRU and beats state-of-the-art eviction policies on 45%+ of 1,559 real-world cache traces. Colander puts it into a production-ready proxy you can deploy in front of any HTTP backend.

```
docker compose up    # start proxy + backend + load generator + dashboard
open http://localhost:3001   # watch SIEVE outperform LRU in real time
```

## Why SIEVE

Traditional caches use LRU, which moves every accessed item to the front of a linked list. This requires a **write lock on every cache hit** — a scalability bottleneck on multi-core systems.

SIEVE replaces this with a single bit flip (`AtomicBool`) on hit. A roving "hand" pointer handles eviction by scanning from tail to head, keeping visited items in place and evicting cold ones. The result:

| Property | LRU | SIEVE |
|----------|-----|-------|
| Cache hit operation | Move-to-front (write lock) | Flip visited bit (lock-free) |
| Eviction strategy | Always evict tail | Hand scans for unvisited |
| Miss ratio | Baseline | Up to 63% lower than ARC |
| Multi-thread scaling | Limited by write contention | Near-linear |

## Architecture

```
┌─────────────┐     ┌─────────────────────────────┐     ┌─────────────┐
│  Load Gen   │────▶│       Colander Proxy        │────▶│   Backend   │
│  (Zipfian)  │◀───│                             │     │             │
└─────────────┘ α  │  ┌────────┐  ┌────────┐    │     └─────────────┘
  (adjustable)     │  │ SIEVE  │  │  LRU   │    │
                   │  │ cache  │  │ (comp) │    │
                   │  └────────┘  └────────┘    │
                   │      │ WebSocket metrics    │
                   └──────┼─────────────────────┘
                          │
                   ┌──────▼──────────────┐
                   │     Dashboard       │
                   │  SIEVE vs LRU live  │
                   └─────────────────────┘
```

**Dual-cache mode**: every request is checked against both SIEVE and LRU. Responses are served from SIEVE; LRU runs in shadow mode for a fair, same-traffic comparison. Toggle to bench mode for true single-policy throughput numbers.

## Cache Design

The `colander-cache` crate implements three eviction policies behind a common `CachePolicy` trait:

- **SIEVE** — hand pointer + `AtomicBool` visited bit, no list mutation on hits
- **LRU** — move-to-front on every access (write lock required)
- **FIFO** — insert at head, evict from tail, no promotion

All three use an **arena-allocated doubly-linked list** — nodes stored in a `Vec<Option<Node>>` with `u32` indices instead of pointers. This keeps the borrow checker happy, avoids `unsafe`, and is cache-line friendly.

**Sharding**: `ShardedCache<T>` distributes keys across 64 independent shards via `ahash`. Each shard has its own `parking_lot::RwLock`, arena, and eviction state. On a cache hit, only 1/64th of the cache is locked.

## Project Structure

```
colander/
├── crates/
│   ├── colander-cache/     # Cache library: SIEVE, LRU, FIFO, sharded wrapper
│   ├── proxy-server/       # HTTP reverse proxy with cache layer + metrics
│   ├── loadgen/            # Zipfian traffic generator with adjustable α
│   └── demo-backend/       # Fake origin API with artificial latency
├── dashboard/              # React + Recharts live metrics UI
├── docker/                 # Dockerfiles
└── docker-compose.yml      # One-click demo
```

## Quick Start

### From source

```bash
# Start the demo backend
cargo run -p demo-backend &

# Start the proxy (caches requests to the backend)
cargo run -p proxy-server

# In another terminal
curl -v http://localhost:8080/api/items/42   # X-Cache: MISS
curl -v http://localhost:8080/api/items/42   # X-Cache: HIT
```

### Docker

```bash
docker compose up --build
# Dashboard: http://localhost:3001
# Proxy:     http://localhost:8080
# Metrics:   ws://localhost:9090/ws/metrics
```

## Configuration

```toml
[server]
listen_addr = "0.0.0.0:8080"
metrics_addr = "0.0.0.0:9090"

[upstream]
url = "http://localhost:3000"
timeout_ms = 5000

[cache]
capacity = 10000
default_ttl_seconds = 60
max_body_size_bytes = 1048576
eviction_policy = "sieve"
comparison_policy = "lru"
```

## Running Tests

```bash
cargo test                         # all crates
cargo test -p colander-cache       # cache library only
cargo bench -p colander-cache      # SIEVE vs LRU throughput
```

## References

- [SIEVE: Simpler than LRU (NSDI '24 paper)](https://www.usenix.org/conference/nsdi24/presentation/zhang-yazhuo)
- [SIEVE project page](https://cachemon.github.io/SIEVE-website/)

## License

MIT
