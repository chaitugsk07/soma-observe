# soma-observe: Install & UX Design

**Date:** June 2026

soma-observe is a single Rust binary that receives OTLP/HTTP telemetry (metrics and logs, v1) and exposes a small OTel-faithful JSON query API for reading that data back. The install experience is: start one compose file, point your OTel SDK exporter at port 4318, query via the API using curl, your SDK, or any HTTP client. "One step" here means one command to a running, queryable observability backend — not zero dependencies, but zero new dependencies beyond the Postgres you probably already run. There is no bundled UI and no Grafana dependency in v1.

---

## 1. What "one-step install" actually means here

The binary needs a Postgres database. That is the one honest dependency. It is not an embedded store.

That choice was deliberate. Every soma-platform service (soma-vault, soma-iam, soma-audit) already connects to a shared Postgres via `soma_infra::connect_from_env()`. soma-observe is one more consumer of that pool — no new stateful dependency, no new backup strategy, no new operational skill required. The tradeoff: unlike OpenObserve or GreptimeDB in standalone mode, soma-observe cannot claim truly zero external dependencies. What it claims instead is that you already have the dependency if you run any soma service (see [competitive-analysis.md](./competitive-analysis.md) for why fully embedded storage was ruled out; the short answer is platform consistency and reusing the pool every service already runs).

For users outside the soma-platform who are evaluating fresh: the truly one-command path is `docker compose up -d` from the committed compose file, which brings up Postgres and soma-observe together.

---

## 2. The actual one-liners

### Primary / truly one-step: docker compose

```bash
docker compose up -d
```

This is the documented default. It brings up a Postgres container and a soma-observe container as a single unit. No existing Postgres required.

Minimal `docker-compose.yml`:

```yaml
version: "3.9"
services:
  postgres:
    image: postgres:16-alpine
    environment:
      POSTGRES_DB: soma
      POSTGRES_USER: soma
      POSTGRES_PASSWORD: soma
    volumes:
      - postgres_data:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U soma"]
      interval: 5s
      timeout: 5s
      retries: 5

  soma-observe:
    image: ghcr.io/chaitugsk07/soma-observe:latest
    depends_on:
      postgres:
        condition: service_healthy
    environment:
      DATABASE_URL: postgres://soma:soma@postgres:5432/soma
      LISTEN_ADDR: "0.0.0.0:4318"
    ports:
      - "4318:4318"   # OTLP/HTTP ingest + OTel-faithful JSON query API

volumes:
  postgres_data:
```

Why this is the default: it is the fastest path from zero to a working backend, it ships as a single file alongside the binary, and it is self-contained for evaluation. Moving to your own Postgres later is one env-var change.

### docker run — for users who already have Postgres

```bash
docker run -d \
  -e DATABASE_URL="postgres://user:pass@your-host:5432/soma" \
  -e LISTEN_ADDR="0.0.0.0:4318" \
  -p 4318:4318 \
  ghcr.io/chaitugsk07/soma-observe:latest
```

This is the soma-platform deployment path: the platform already runs Postgres, so you point soma-observe at the shared `DATABASE_URL`. Migrations run automatically at startup and create the `soma_observe` schema.

### Binary install — for the binary-first crowd

Via install script:

```bash
curl -fsSL https://github.com/chaitugsk07/soma-observe/releases/latest/download/install.sh | sh
DATABASE_URL="postgres://..." soma-observe
```

Or from crates.io (once published):

```bash
cargo install soma-observe
DATABASE_URL="postgres://..." soma-observe
```

Build from source (profile.dev has `debug = false` and `strip = true` per platform convention):

```bash
cargo build --release
DATABASE_URL="postgres://..." ./target/release/soma-observe
```

---

## 3. What it stands up

One process, one dependency (Postgres). The binary runs a single HTTP listener on `:4318` serving two route groups:

```
OTel SDK / Collector
        |
        | POST /v1/metrics, /v1/logs  (OTLP/HTTP protobuf)
        v
  soma-observe :4318
  ┌─────────────────────────────────────────┐
  │ ingest:  POST /v1/metrics               │
  │          POST /v1/logs                  │
  │                                         │
  │ query:   GET /api/v1/metrics/query      │
  │          GET /api/v1/logs/query         │
  └─────────────────────────────────────────┘
        |                   ^
        | sqlx INSERT        | SELECT
        v                   |
  Postgres (schema: soma_observe)
  ┌───────────────────────────────────┐
  │  metric_series                    │
  │  metric_point (range-partitioned) │
  │  metric_histogram_point (range-p) │
  │  logs                             │
  └───────────────────────────────────┘
        ^
        | GET /api/v1/metrics/query
        | GET /api/v1/logs/query
        |
  API consumer (curl / SDK / your tooling)
```

At startup, soma-schema runs migrations under the `soma_observe` schema (advisory lock key unique to this service). Once migrations complete, the listener opens. There is nothing else: no sidecar, no agent, no config file beyond env vars.

Splitting ingest and query onto separate ports and adding auth is a v2 hardening item; ingest and read share a single port for simplicity in v1.

### Postgres schema shape (v1)

Metrics use a **normalized two-table model**:

- `metric_series` — one row per unique metric series. `series_id` is a stable hash of `(metric_name, resource_attributes, datapoint_attributes)`. Columns: `series_id`, `name`, `resource jsonb`, `attributes jsonb`, `kind` (gauge | sum | histogram), `unit`. Stored once; never duplicated per data point.
- `metric_point` — one row per scalar data point. Columns: `series_id` (FK → `metric_series`), `ts timestamptz`, `value double precision`. Range-partitioned by `ts` (monthly). Primary index: composite B-tree `(series_id, ts)` — this is what makes per-series time-range queries efficient. BRIN on `ts` is a secondary index used only for bulk export / partition pruning.
- `metric_histogram_point` — one row per histogram data point. Columns: `series_id` (FK), `ts`, `sum`, `count`, `bucket_counts jsonb`, `bounds jsonb`. Range-partitioned by `ts`; same `(series_id, ts)` composite B-tree index. A scalar `value` column cannot represent a histogram; this is a dedicated table for that reason.

Logs use a `body TEXT` column plus `attributes jsonb` with a GIN index. No TimescaleDB — its columnar compression features are under the TigerData License (source-available, not open source), which introduces redistribution risk; plain Postgres partitioning is sufficient at the v1 ceiling.

**Honest ceiling:** ~5–10 million metric data points per day and ~10 GB of logs before sequential-scan query times become user-noticeable. That is roughly 60–115 samples/second of continuous ingest. This covers the large majority of small/medium self-hosted deployments. It is documented openly, not hidden. When you hit the ceiling, the v3 path (see section 8) swaps the storage backend without changing the ingest or query contracts.

---

## 4. How data gets in

OTLP/HTTP only, port 4318. This is the single ingest path in v1.

### Point an OTel SDK exporter at soma-observe

In most SDKs, set the OTLP exporter endpoint to `http://<host>:4318`. Example for the Rust OTel SDK via env var:

```bash
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 \
OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf \
  your-service
```

### Point an OTel Collector at soma-observe

```yaml
# otelcol-config.yaml
exporters:
  otlphttp:
    endpoint: "http://soma-observe:4318"

service:
  pipelines:
    metrics:
      exporters: [otlphttp]
    logs:
      exporters: [otlphttp]
```

### Send a test metric with otel-cli

```bash
otel-cli span \
  --endpoint http://localhost:4318 \
  --protocol http/protobuf \
  --name "test-metric"
```

Or use `grpcurl` / raw HTTP with a protobuf payload for manual verification:

```bash
# Send a minimal OTLP/HTTP metrics payload (JSON encoding, accepted by soma-observe)
curl -X POST http://localhost:4318/v1/metrics \
  -H "Content-Type: application/json" \
  -d '{"resourceMetrics":[]}'
# Expect: 200 OK with {"partialSuccess":{}}
```

### OTLP metric types accepted in v1

soma-observe v1 accepts three OTLP metric types: **Gauge**, **Sum** (cumulative temporality only), and **Histogram** (explicit-bucket, cumulative temporality only). ExponentialHistogram and delta-temporality data are rejected at ingest with an OTLP `partial_success` response and will be handled in v2. Note: even GreptimeDB defers ExponentialHistogram support.

### Cumulative-to-delta conversion at ingest

OTLP Sum (and Histogram) data arrives with cumulative temporality: each data point carries the total since the process started, not the increment since the last point. Storing raw cumulative values and summing them at query time is incorrect — it double-counts all points and produces wrong totals after a process restart.

soma-observe converts cumulative values to per-series deltas at ingest time. An in-process cache holds the last observed value per `series_id`; the delta is computed as `current − previous`. When the current value is less than the previous (counter reset on process restart), the current value is used as-is for that interval. Deltas are what get written to `metric_point`. This keeps the read path as plain SQL with no conversion logic.

### Partition lifecycle

Partitions are managed by soma-observe itself — no TimescaleDB, no pg_partman, no extensions. A task runs at startup and on a periodic interval: it CREATEs the next month's partition ahead of the current boundary and DROPs any partition older than `*_RETENTION_DAYS`. Retention is partition DROP, not row DELETE.

**Invariant:** the next partition must exist before the first data point with a timestamp in that range is written. A missing partition at the boundary is an ingest outage. The create-ahead task must run with enough lead time to guarantee the partition exists.

### Ingest robustness

On write, soma-observe acquires a sqlx pool connection with a configured acquire timeout and applies a statement timeout. If the write exceeds the timeout, soma-observe returns HTTP 503 with a `Retry-After` header; the caller (OTel SDK or Collector) can retry with exponential backoff. Data points with timestamps older than the ingest window are rejected with HTTP 400 — late/out-of-order points that would land in an already-dropped partition are not accepted.

**Cardinality guidance.** Each unique `(metric_name, resource_attributes, datapoint_attributes)` combination is a new row in `metric_series`. High-cardinality resource attributes — `k8s.pod.uid`, `process.pid`, `container.id` — create a new series for every pod/process lifecycle, which bloats `metric_series` rapidly. Strip these attributes in the OTel Collector before they reach soma-observe using a `transform/attributes` processor. The Collector is the right place to apply this filter; it requires no changes to the instrumented services.

### Exporter caveat

> **Prometheus exporter ecosystem gap (v1 honest limitation)**
>
> Existing Prometheus exporters — `node_exporter`, `postgres_exporter`, `blackbox_exporter`, and the broader exporter ecosystem — speak the Prometheus scrape format or Prometheus remote-write protocol, not OTLP. They cannot point at soma-observe directly in v1.
>
> Two options for users with exporter-based infrastructure: (a) run an OTel Collector with a `prometheusreceiver` in front of soma-observe — this adds one process but requires no changes to existing exporters; (b) wait for the v2 Prometheus remote-write receiver, which closes this gap without a collector hop.
>
> v1 is the right fit for services instrumented with the OTel SDK from day one. It is not a transparent drop-in for a Prometheus-scrape-based setup.

---

## 5. How a user sees it — the OTel-native query API

soma-observe does not bundle a UI. The read layer is a small HTTP query API served on the same `:4318` listener as OTLP ingest. You query it with curl, an HTTP client in your SDK of choice, or any tooling that speaks plain JSON.

OTLP is an ingest-only protocol — OpenTelemetry defines no query or read standard — so the read side is necessarily a custom API. A Prometheus-compatible API was considered but rejected: it would force OTLP data into Prometheus's metric model, flattening resource attributes into a single label set, losing exponential histogram fidelity, discarding severity and trace correlation, and generally trading the full OTLP data model for compatibility with tooling that was not designed around it. The native API below preserves the OTLP structure end-to-end.

### Endpoints

| Endpoint | Method | Description |
|---|---|---|
| `GET /api/v1/metrics/names` | GET | List all metric names known to this instance |
| `GET /api/v1/metrics/series` | GET | List active series for a given metric name |
| `GET /api/v1/metrics/query` | GET | Time-range metric query with aggregation |
| `GET /api/v1/logs/query` | GET | Time-range log query with filter and body search |

`GET /api/v1/metrics/names` and `GET /api/v1/metrics/series` are necessary for operability: without them, with no UI and no PromQL browser, there is no way to discover what data has been ingested or what attribute combinations exist.

### Metrics query

`GET /api/v1/metrics/query`

Query parameters:

| Param | Description |
|---|---|
| `name` | Metric name (required) |
| `start`, `end` | Unix seconds or RFC3339 |
| `step` | Bucket size in seconds |
| `filter` | Attribute selector, e.g. `service.name="api",http.method="GET"` |
| `agg` | Aggregation: `sum`, `avg`, `min`, `max`, or `count` |

When `agg` and `step` are provided, aggregation runs server-side using `GROUP BY date_bin($step, ts, <origin>)` in Postgres. `date_bin()` is the standard Postgres function (not TimescaleDB's `time_bucket`). For Sum series (counters), the stored deltas are summed per bucket to produce a rate-equivalent output — the response will note the series kind so callers know they are reading derived deltas, not raw cumulative values.

Response preserves OTLP structure — resource attributes and data-point attributes are kept separate, not flattened to a single label set:

```json
{
  "metric": "http.server.duration",
  "unit": "ms",
  "series": [
    {
      "resource": { "service.name": "api", "host.name": "node-1" },
      "attributes": { "http.method": "GET", "http.route": "/v1/things" },
      "points": [ { "start": 1719532800, "end": 1719532860, "value": 42.0, "count": 10 } ]
    }
  ]
}
```

### Logs query

`GET /api/v1/logs/query`

Query parameters:

| Param | Description |
|---|---|
| `start`, `end` | Unix seconds or RFC3339 |
| `filter` | Attribute selector, including `service.name`, `trace_id`, `span_id`, and arbitrary attributes |
| `severity` | Minimum severity level, e.g. `WARN` |
| `q` | Substring match on log body |
| `limit` | Maximum records returned |

Returns newline-delimited JSON with full OTLP log-record fidelity:

```json
{"timestamp":"2026-06-28T10:00:00Z","severity":"ERROR","severity_number":17,"body":"connection timeout","trace_id":"4bf92f...","span_id":"00f067...","resource":{"service.name":"api"},"attributes":{"net.peer.name":"db"}}
```

You can now script against the query API directly — no Grafana, no plugin, no intermediary required.

---

## 6. Configuration

All configuration is via environment variables, read through `soma_infra::config` (`require_env`, `env_or`, `env_parse`).

| Variable | Default | Notes |
|---|---|---|
| `DATABASE_URL` | — | Required. Postgres connection string. `require_env` — fails fast at startup if absent. |
| `LISTEN_ADDR` | `0.0.0.0:4318` | Single HTTP listener for both OTLP/HTTP ingest (`POST /v1/metrics`, `POST /v1/logs`) and the OTel-faithful JSON query API (`GET /api/v1/...`). Port 4318 is the OTLP standard. |
| `METRICS_RETENTION_DAYS` | `90` | Partition drops older than this many days. Applied on startup and via a periodic job. |
| `LOGS_RETENTION_DAYS` | `30` | Same for logs. Logs tend to be larger; shorter default. |
| `RUST_LOG` | `info` | Log level. Standard `RUST_LOG` / `tracing` env filter. |
| `AUTH_TOKEN` | unset | If set, all ingest and query requests must include `Authorization: Bearer <token>`. Enforced via `soma_infra::web::extract_bearer`. If unset, the listener is open (suitable for localhost / trusted-network quickstart). At startup, if soma-observe binds a non-loopback address and `AUTH_TOKEN` is not set, a loud warning is emitted to stderr. |

That is the complete surface. No config file, no YAML, no feature flags. Additional knobs can be added in later versions as real need emerges; starting minimal avoids locking in a configuration API before usage patterns are understood.

Splitting ingest and query onto separate listen addresses and adding per-route authentication is a v2 hardening item.

---

## 7. Success in 60 seconds

After running `docker compose up -d`:

1. **Verify the containers are up.**
   ```bash
   docker compose ps
   ```
   Both `postgres` and `soma-observe` should show `running (healthy)` or `running`.

2. **Check soma-observe logs for migration success.**
   ```bash
   docker compose logs soma-observe
   ```
   Look for: `soma_observe schema migrations applied` and `listening on 0.0.0.0:4318`.

3. **Send a test metric.**
   ```bash
   curl -s -X POST http://localhost:4318/v1/metrics \
     -H "Content-Type: application/json" \
     -d '{"resourceMetrics":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"smoke-test"}}]},"scopeMetrics":[{"metrics":[{"name":"test_counter","sum":{"dataPoints":[{"asInt":"1","timeUnixNano":"'$(date +%s%N)'","attributes":[{"key":"env","value":{"stringValue":"local"}}]}],"aggregationTemporality":2}}]}]}]}'
   ```
   Expect: HTTP 200 with `{"partialSuccess":{}}`.

4. **Query it back.**
   ```bash
   curl -s "http://localhost:4318/api/v1/metrics/query?name=test_counter&start=$(date -v-5M +%s)&end=$(date +%s)"
   ```
   Expect: HTTP 200 with a JSON body containing the metric you just sent.

5. **Send a test log entry.**
   ```bash
   curl -s -X POST http://localhost:4318/v1/logs \
     -H "Content-Type: application/json" \
     -d '{"resourceLogs":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"smoke-test"}}]},"scopeLogs":[{"logRecords":[{"timeUnixNano":"'$(date +%s%N)'","body":{"stringValue":"hello from soma-observe"},"attributes":[{"key":"level","value":{"stringValue":"info"}}]}]}]}]}'
   ```
   Expect: HTTP 200.

6. **Query it back.**
   ```bash
   curl -s "http://localhost:4318/api/v1/logs/query?start=$(date -v-5M +%s)&end=$(date +%s)&filter=service.name=\"smoke-test\""
   ```
   Expect: newline-delimited JSON with your log line.

You can now script against the query API — curl it, call it from your SDK, or wire it into any HTTP-capable tooling.

**"It worked" signal:** steps 3 and 4 both return HTTP 200 with data. Everything else is confirmation.

---

## 8. Phased install evolution

### v1 — current (metrics + logs, OTLP/HTTP, Postgres)

Single binary. Single HTTP listener on :4318. Postgres via soma-infra pool. OTel-faithful JSON query API (`GET /api/v1/metrics/names`, `GET /api/v1/metrics/series`, `GET /api/v1/metrics/query`, `GET /api/v1/logs/query`). Metrics (Gauge, Sum, explicit-bucket Histogram; cumulative temporality; deltas computed at ingest) and logs only. No bundled UI. soma-schema migrations auto-run at startup under `soma_observe` schema. Optional bearer token auth via `AUTH_TOKEN`.

Stable surface: the OTLP/HTTP ingest contract and the HTTP query API shape. These do not change in v2 or v3.

### v2 — OTLP/gRPC + Prometheus remote-write + traces + optional UI

- OTLP/gRPC on :4317 via tonic (soma-infra adds tonic in v2 or soma-observe carries it directly).
- Prometheus remote-write receiver: accepts `/api/v1/write` from any Prometheus instance or Prometheus-compatible exporter. This is what closes the legacy exporter gap — `node_exporter`, `postgres_exporter`, and the rest can point at soma-observe without an OTel Collector hop.
- Trace ingest: OTLP spans stored as rows in Postgres with `trace_id`, `span_id`, `parent_span_id`, and `attributes jsonb`. A `/api/v1/query/traces` endpoint filters by trace ID and time range, returns all spans. No TraceQL in v2, no Gantt chart — just the retrieval layer.
- Optional minimal built-in read-only UI (candidate feature: since v1 has no bundled UI and no Grafana dependency, v2 is the right time to evaluate a lightweight embedded dashboard if the API-only story proves insufficient for the target user).
- Storage schema is additive; no migration from v1 data.

### v3 — object storage tier (when the Postgres ceiling is hit)

When ingest consistently exceeds the v1 Postgres ceiling (~5–10M metric points/day, ~10 GB logs), the write path switches to DataFusion + Parquet via `soma_infra::storage::StorageClient` (the `storage-s3` or `storage-azure` feature, built on `object_store` 0.12). Hot recent data stays in Postgres (last N hours); cold data moves to object storage.

Configuration change: add `OBJECT_STORAGE_URL` (or S3/Azure equivalents). No application code changes required for operators; the OTLP ingest contract and the HTTP query contract remain identical. Storage is an implementation detail, not part of the API surface.

Do not add DataFusion in v1 or v2. Making the FDAP (Flight, DataFusion, Arrow, Parquet) stack production-ready without a WAL and compaction engine is ~30,000 lines of Rust. That is the opposite of the simplicity mandate. See [competitive-analysis.md](./competitive-analysis.md) for InfluxDB 3 Core as the reference data point.

---

## 9. soma-infra / soma-schema consumption

What soma-observe consumes from shared platform infrastructure vs. what it brings itself:

### Consumed from soma-infra

| Concern | Use | Feature |
|---|---|---|
| Postgres pool | `soma_infra::connect_from_env()` | `db` (default) |
| Telemetry / logging | `soma_infra::telemetry::init()` | `tracing` (default) |
| Env-var helpers | `soma_infra::config::{require_env, env_or, env_parse}` | `config` |
| Graceful shutdown | `soma_infra::signal::shutdown_signal()` | `signal` |
| HTTP server | `soma_infra::web::serve_with_shutdown(addr, app)` | `web` |
| Auth (bearer token) | `soma_infra::web::extract_bearer` | `web` |

### Consumed from soma-schema

| Concern | Use |
|---|---|
| Schema migrations | `soma_observe` schema name; unique advisory lock key; `migrations/` dir with UP/DOWN files; run-at-startup |

The schema name (`soma_observe`) and the advisory lock key are service-owned policy, not plumbing — they live in soma-observe, not soma-infra, consistent with how soma-vault and soma-iam handle their own schema names and lock keys.

### Brought by soma-observe itself

| Concern | Crate / Implementation |
|---|---|
| OTLP protobuf decode | `opentelemetry-proto` (generated types for `MetricsData`, `LogsData`) |
| OTLP/HTTP route handlers | soma-observe axum handlers (`POST /v1/metrics`, `POST /v1/logs`) |
| OTel-faithful JSON query API | soma-observe: `serde_json` over stored OTLP fields for `GET /api/v1/metrics/names`, `GET /api/v1/metrics/series`, `GET /api/v1/metrics/query`, `GET /api/v1/logs/query`; full resource/attribute fidelity, not flattened to a single label set |
| Metric write path | soma-observe: decode OTLP proto → upsert `metric_series` → convert cumulative to delta → insert into `metric_point` or `metric_histogram_point` |
| Log write path | soma-observe: extract from OTLP proto → insert into `soma_observe.logs` |
| Partition lifecycle | soma-observe: startup + periodic task; CREATEs next partition ahead of the boundary, DROPs partitions older than `*_RETENTION_DAYS`; retention is partition DROP, not row DELETE |
| Storage schema / partition policy | soma-observe migrations: normalized `metric_series` + `metric_point` + `metric_histogram_point` tables, range-partitioned by `ts`, composite B-tree `(series_id, ts)` as primary index, BRIN `(ts)` as secondary — service-owned policy |

soma-infra does not provide OTLP decoding, gRPC, or the OTel-faithful query serialization. Those are not generic plumbing; they are soma-observe's application logic.

Cargo dependency declaration (path form for monorepo, crates.io form for standalone builds):

```toml
[dependencies]
soma-infra = { path = "../../../soma-infra", features = ["db", "tracing", "config", "signal", "web"] }
opentelemetry-proto = { version = "0.7", features = ["metrics", "logs", "with-serde"] }
axum = "0.8"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
prost = "0.13"

[profile.dev]
debug = false
strip = true
```
