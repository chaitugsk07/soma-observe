# soma-observe: Competitive Analysis

**Date:** June 2026

The self-hosted observability market has fragmented into two camps: complex multi-process stacks with rich feature sets, and single-binary tools that cover only one signal. No credible open-source project delivers all three signals (metrics, logs, traces) in a genuine single binary backed by a storage layer a small team can actually operate. That gap is the only honest wedge for soma-observe. The recommended v1 is deliberately narrow — OTLP metrics and logs stored in Postgres — and this document explains exactly why.

---

## 1. The Incumbent Stack We Are Replacing

### Prometheus + Loki + Tempo + Grafana (LGTM/PLGT)

**What each piece does.** Prometheus scrapes metrics from instrumented targets, stores them in a custom on-disk time-series database (TSDB), and exposes PromQL for queries. Version 3.0 shipped November 14, 2024 — the first major release since 2017 — adding native OTLP metrics ingest, Remote Write 2.0, UTF-8 label names, and a rewritten UI. Loki ingests logs, indexes only labels (not full text), stores compressed chunks in object storage, and queries via LogQL. Version 3.0 (April 8, 2024) made the TSDB+v13 schema the default and deprecated BoltDB. Tempo accepts distributed trace spans over OTLP, Jaeger, or Zipkin, stores them as Apache Parquet blocks on object storage, and queries via TraceQL. Grafana is the UI layer: a dashboard builder with datasource plugins that fan queries out to all three backends, plus an alerting engine.

**Why running the combo is painful.** The minimum viable self-hosted stack is five separate processes: Prometheus, Loki, Tempo, Grafana, and a Grafana Alloy collector (the replacement for Grafana Agent, which reached end-of-life in November 2025). That is five separate config files, five upgrade cycles, and five places to check when something breaks.

Each backend has its own query language with incompatible syntax: PromQL for metrics, LogQL for logs, TraceQL for traces. Correlating across signals — jumping from a metric spike to the relevant logs to the causal trace — requires manual label alignment and clicking through three Grafana datasources. The 2025 Grafana Observability Survey (n=1,255) found 39% of respondents cited setup and maintenance complexity as their single biggest obstacle.

Each backend has different storage requirements. Prometheus needs local SSD for its TSDB head block. Loki requires object storage (S3, GCS, or Azure Blob) for chunks in production. Tempo requires object storage and explicitly does not support local disk in distributed production mode. Cardinality explosions in Prometheus — unbounded label values — consume RAM proportionally and can crash the process. Scaling past a single Prometheus node requires adding Thanos or Grafana Mimir, both of which add further processes and configuration surfaces.

Grafana, Loki, and Tempo were relicensed from Apache 2.0 to AGPLv3 in April 2021. Prometheus remains Apache 2.0. The AGPLv3 applies to network use: any modification made available over a network must be published. For pure self-hosters this is largely moot; for teams building a product on top, it is a real constraint.

The `grafana/docker-otel-lgtm` all-in-one container bundles all components and can idle within roughly 2 GB of RAM, but Grafana explicitly documents it as a development and testing image, not a production artifact.

---

## 2. The All-in-One Challengers

### SigNoz

SigNoz is an open-source APM and observability backend written primarily in Go (37%) and TypeScript/React (53%), with 27,500+ GitHub stars as of mid-2026. It covers all three signals and uses OTLP as its native ingestion protocol on gRPC port 4317 and HTTP port 4318.

**Storage.** ClickHouse for all telemetry data. PostgreSQL (version 16) for metadata. ClickHouse Keeper for coordination in the default Docker install (though production capacity-planning documentation still references ZooKeeper for HA deployments, with migration pending). There is no option to substitute Postgres for telemetry storage.

**Single binary claim.** As of v0.76 (March 2025), the SigNoz application — UI, API server, alertmanager, OpAMP server, and ruler — ships as one binary. ClickHouse, ClickHouse Keeper, and PostgreSQL remain separate containers. The practical install is five containers via Docker Compose: the SigNoz binary, an OTel Collector, ClickHouse server, ClickHouse Keeper, and PostgreSQL. Minimum RAM allocation documented as 4 GB. "Single binary" here means the application layer only; the storage dependencies are not embedded.

**Query model.** A custom visual query builder that generates ClickHouse SQL. PromQL is available as an additional option for metrics. Direct ClickHouse SQL is exposed for advanced use.

**License.** Dual-license: MIT for the core codebase outside `ee/` and `cmd/enterprise/`; proprietary SigNoz Enterprise license for those directories. No BSL, SSPL, or AGPL as of mid-2026. The community edition is usable without the enterprise code; SSO/SAML and other enterprise features require a paid license. Install tooling migrated to a tool called "Foundry" introduced at v0.112.0 (February 18, 2026).

**Verdict for soma-observe.** SigNoz is substantially simpler than the LGTM stack as a single project with one install path. It is not lightweight. Operating it means operating ClickHouse, which brings real tuning overhead. The three-to-five-container footprint idles at roughly 3–5 GB RAM on a reasonably sized node, an approximation from community install guides rather than a SigNoz-published figure.

---

### Uptrace

Uptrace is an open-source APM backend written in Go (54%) with a Vue/TypeScript frontend (43%). It ingests all three signals natively over OTLP/gRPC (port 4317) and OTLP/HTTP (port 4318).

**Storage.** ClickHouse for spans, logs, and metrics. PostgreSQL (minimum v14) for metadata. Redis became a mandatory dependency in v2.0.0 (July 24, 2025), required for caching and session management. There is no embedded storage option. The v2.0.0 release rewrote both the ClickHouse and PostgreSQL schemas, requiring a non-trivial migration from earlier installs. The v2.1.0-beta series (January 2026) raised the minimum ClickHouse version to v25.8+ and shifted to explicit pre-startup migration steps rather than auto-migration.

**Install reality.** Uptrace ships as a single Go binary, but the practical minimum is six services in the official Docker Compose: Uptrace, ClickHouse, PostgreSQL, Redis, an OTel Collector, and MailHog (email, technically optional). Calling this a four-container stack understates it.

**License.** AGPL-3.0, changed from BSL 1.1 at v1.6.0 (January 3, 2024). The free Community edition caps data retention at 14 days; the enforcement mechanism and whether this applies to self-hosted or only cloud was not definitively confirmed from official documentation. Paid tiers progressively unlock 2FA (Starter, $39/month), RBAC (Team), SSO (Business), and audit logs (Enterprise).

**Query model.** Custom DSL for spans and logs (filter, group-by, aggregate). PromQL-compatible for metrics — the docs describe it as "aims to be compatible... while extending it," so partial compatibility rather than full PromQL. No TraceQL.

**Verdict for soma-observe.** Uptrace is the right shape — OTLP-native, all-in-one signals, AGPL — but the wrong stack depth. Three mandatory external databases plus an OTel Collector is the antithesis of "one-step install." The smallest credible delta from Uptrace's model is eliminating ClickHouse by using Postgres for telemetry at small/medium ingest volumes, and dropping Redis by holding hot query state in-process. That is effectively the soma-observe v1 thesis.

---

### OpenObserve (o2)

OpenObserve is the closest structural analog to what soma-observe is trying to build, written in Rust for the backend with a Vue/TypeScript/JavaScript frontend totaling roughly 70% of the monorepo by language share.

**Storage.** Apache Parquet files on any S3-compatible object store, or local disk for single-node. Apache DataFusion is the query engine reading Parquet directly. Single-node mode uses SQLite for metadata and requires no external dependencies. Cluster/HA mode requires NATS for coordination, PostgreSQL for metadata, object storage, and separate Ingester, Compactor, Querier, and AlertManager processes.

**Single-binary — real but qualified.** For a single-node setup the promise is genuine: download one binary, set two environment variables (`ZO_ROOT_USER_EMAIL`, `ZO_ROOT_USER_PASSWORD`), run. The simplicity story breaks down entirely for production HA, which is a real multi-process distributed system. The UI is a substantial bundled Vue SPA — not a minimal service.

**Signals.** Logs, metrics, traces, RUM, frontend monitoring, and LLM observability. The broadest signal coverage of any open-source project.

**License.** AGPL-3.0, changed from Apache 2.0 in November 2023. Enterprise-only features include SSO/OIDC/SAML, granular RBAC, audit trail, federated search, and BYOB storage. OpenObserve raised a $10M Series A in April 2026.

**Query model.** SQL for logs and traces. PromQL for metrics with known bugs: PromQL arithmetic across two or more metrics returned "No Data" in v0.13 (January 2025 report, subsequently addressed via PR #5719). A case-sensitivity bug and an alert builder PromQL parsing error appeared as late as v0.15.1 (October 2025). The research characterization of PromQL as "in progress, not finished" remained valid through at least late 2025.

**Verdict for soma-observe.** OpenObserve proves the Rust + Parquet + single binary model works. The key differences soma-observe could offer: Postgres-native storage (no separate object store requirement at small deployments), integration with soma-infra plumbing already present in the soma-platform monorepo, and a deliberately narrower scope. Trying to match OpenObserve's breadth — RUM, pipelines, LLM observability, federated search — would be scope creep that violates the simplicity mandate.

---

### Grafana Stack: Mimir / Loki / Tempo / Pyroscope / Alloy

The complete Grafana Labs observability suite. See section 1 for the baseline picture. Additional detail on the component stack:

**Mimir** is the horizontally scalable Prometheus-compatible metrics store, forked from the now-maintenance-mode Cortex, launched under AGPLv3 in March 2022. Its monolithic `-target=all` mode runs all components in one process but is explicitly documented as not recommended for production at any meaningful scale. Version 3.0 (November 2025) added Kafka-based decoupled ingestion.

**Pyroscope** (continuous profiling) made object storage mandatory for distributed deployments in version 2.0 (April 2026). Single-node monolithic deployments can still use local filesystem; the mandatory-object-storage claim applies only to distributed mode.

**Grafana Alloy** replaced both Grafana Agent (EOL November 2025) and Promtail (EOL March 2026). It is a first-class OpenTelemetry Collector distribution, licensed Apache 2.0.

**Mimir OTLP ingest.** Mimir exposes a native OTLP HTTP endpoint directly — it does not strictly require Alloy as an intermediary, though Alloy is the common pipeline choice.

**Verdict for soma-observe.** This is the incumbent soma-observe is displacing, not a model to emulate. The multi-process, multi-language-query, multi-object-store architecture is the exact pain point the project addresses. The AGPLv3 on Grafana, Loki, Tempo, Mimir, and Pyroscope server is a real constraint for anyone building a product on top.

---

### VictoriaMetrics + VictoriaLogs + VictoriaTraces

All three products from the VictoriaMetrics team are written in Go and licensed Apache 2.0 with an explicit public commitment against BSL or SSPL, and no contributor license agreement requirement — making a license change structurally harder than at Grafana Labs.

**Three separate binaries.** There is no unified binary combining all three signals and none is on any published roadmap. Each component is individually simple; the combination is still three-to-five processes.

**VictoriaMetrics** (metrics, v1.146.0 as of June 2026) is a genuine single binary with no external dependencies and a custom LSM variant storage engine. Query language is MetricsQL, a backwards-compatible PromQL superset. Accepts both the Prometheus scrape/remote-write format and OTLP metrics. Famously 5–10x less RAM than Prometheus for equivalent cardinality workloads.

**VictoriaLogs** (logs) is also a single binary with a custom columnar storage engine. Query language is LogsQL, a Unix-style pipeline format. Accepts logs via OTLP, the Loki push API, Elasticsearch bulk, and several others. Vendor benchmarks claim up to 30x less RAM and 15x less disk than Elasticsearch or Loki; independent third-party benchmarks at equivalent scale were not found.

**VictoriaTraces** (traces, v0.9.3 as of June 18, 2026) is pre-GA. It shares approximately 99% of its storage code with VictoriaLogs, storing trace spans as structured log events. Accepts OTLP/HTTP and OTLP/gRPC. The native web UI is available at `/select/vmui`. Experimental Grafana Tempo HTTP API support exists but the Tempo API is listed as a pre-GA completion requirement, meaning the experimental label is accurate. There is no stable backward-compatibility commitment on the data format before GA.

**Verdict for soma-observe.** For metrics only, VictoriaMetrics is the strongest simple Prometheus replacement available today — arguably the current benchmark for "lightweight single binary." For metrics plus logs, running VM plus VictoriaLogs is two binaries and still much simpler than Prometheus plus Loki. For all three signals, VictoriaTraces is pre-GA with no stability commitment. soma-observe does not need to beat VictoriaMetrics at metrics; it needs to offer a different value: Postgres-native storage that integrates into a platform that already runs Postgres, rather than asking operators to learn a new custom storage engine.

---

### Quickwit

Quickwit is a Rust-native distributed search engine built on Tantivy (a Lucene-inspired full-text search library also written in Rust). Its defining architectural bet is that all index data lives on object storage, with compute nodes stateless.

**Signals.** Logs and traces only. Metrics are a structural gap — the inverted-index architecture is wrong for time-series. A maintainer confirmed in an April 2025 GitHub discussion that metrics remain "still not prioritized."

**Acquisition and license.** Datadog acquired the entire Quickwit team on January 9, 2025, with the founders focused on building internal Datadog products. The Apache 2.0 relicensing promised at acquisition time has shipped — the LICENSE file on the main branch is currently Apache 2.0 (copyright Datadog, Inc.). No new semantic version (v0.9 or higher) has been tagged since v0.8.2 (June 2024). Active commits continue on the main branch, but there is no published independent roadmap.

**Verdict for soma-observe.** Quickwit solves logs and traces well and is well-engineered in Rust. It covers only two of three signals, requires object storage to operate at its design center, and its founding team is now building Datadog products. Treating it as a dependency carries meaningful abandonment risk.

---

### HyperDX / ClickStack

ClickHouse Inc. acquired HyperDX in March 2025 and launched ClickStack in May 2025 — a renamed, officially maintained bundle combining the HyperDX UI, a pre-configured OpenTelemetry Collector, and ClickHouse.

**Signals.** Logs, metrics, traces, and session replay. Session replay is a genuine differentiator with no equivalent in the LGTM stack.

**License.** HyperDX UI: MIT. ClickHouse: Apache 2.0. OTel Collector components: Apache 2.0. No BSL, SSPL, or proprietary cloud-only gating found as of mid-2026. ClickHouse follows an open-core model with some features gated to ClickHouse Cloud, but the core license has not changed.

**Storage.** ClickHouse is the sole storage engine — columnar, OLAP-optimized. Query is Lucene-style syntax plus raw SQL.

**Install reality.** For any production-grade self-hosted deployment, four services are required: ClickHouse, the HyperDX UI (Next.js and Express), an OTel Collector, and MongoDB for persistent application state (dashboards, user accounts, alerts). The all-in-one Docker image bundles all four in a single container, but it runs four processes and is explicitly not production-recommended per official documentation. The "single binary" marketing applies to the embedded ClickHouse read-only exploration variant — no persistence, no alerting, browser session storage only — which is a different artifact from the full ClickStack stack.

**Language.** TypeScript/Node.js for the HyperDX UI (TypeScript is 94% of the hyperdxio/hyperdx repository). ClickHouse is C++. The OTel Collector is Go.

**Verdict for soma-observe.** ClickStack is a credible, well-resourced OTLP-native project. It is not a Rust service, does not use Postgres, and brings ClickHouse plus MongoDB as mandatory heavy dependencies. For the soma-observe target profile (single Rust binary, one Postgres dependency), it is a competitor to study, not a codebase to build on.

---

### GreptimeDB

GreptimeDB is a Rust-written, Apache 2.0-licensed time-series database covering all three OTLP signals in a single binary. It hit v1.0 GA on April 14, 2026; v1.1.1 shipped June 18, 2026. Enterprise features are gated behind a separate commercial license at build time; the core Apache 2.0 codebase has not changed.

**Storage.** Columnar Parquet SST files (the "Flat" format is the default since v1.0). Object storage (S3, GCS, Azure Blob) is the production target via OpenDAL. Standalone mode uses local disk by default. v1.0 GA reported 4x write throughput and 10x query latency improvement on high-cardinality workloads after the Flat format switch.

**Genuine single binary — standalone mode.** Two shell commands, no external dependencies, local disk works out of the box. Ports: HTTP 4000, gRPC 4001, MySQL 4002, Postgres 4003. Distributed mode requires three separate processes: Frontend, Datanode, and Metasrv. Flownode (continuous aggregation) is a fourth but optional component.

**Signals and OTLP gaps.** All three signals via OTLP/HTTP. ExponentialHistogram is explicitly unsupported. Delta-temporality sums and histograms are stored raw without conversion to cumulative. Log field types are restricted: array, float, and object field types error out. Profiles are tracked as a GitHub issue (#6760) and not in the 2026 published roadmap.

**Query model.** SQL (full) and PromQL (queries only, no alerting rules engine). No LogQL equivalent, no TraceQL equivalent — log and trace queries use SQL.

**Bundled dashboard.** The v1.0 binary embeds Perses (CNCF incubating project, v0.12.0). Trace Gantt views and PromQL/SQL panels are included. Perses is substantially less feature-complete than Grafana but it is a bundled UI — soma-observe could satisfy the "avoid Grafana" goal by pointing users here rather than building any UI.

**Verdict for soma-observe.** GreptimeDB is the most directly competitive project to soma-observe's stated goal. It is Rust, single binary, Apache 2.0, and handles all three signals. Its known gaps — PromQL completeness, ExponentialHistogram, SQL-only log querying, Perses maturity — are the places soma-observe could differentiate. But the honest read is: GreptimeDB is a viable choice for many users soma-observe is targeting. soma-observe needs a tighter integration story (soma-infra Postgres pool, soma-schema migrations, the soma-platform ecosystem) rather than trying to out-feature GreptimeDB.

---

### Others Worth Noting

**Parseable** (Rust, AGPL-3.0, v2.9.3 as of June 2026): Rust-native logs/metrics/traces backend storing Parquet on any S3-compatible object store with DataFusion as query engine. Single binary, OTLP-native, bundled dashboards and alerting. Most structurally similar to soma-observe's intended architecture. The object-storage-first design means local-disk-only deployments fall back to MinIO or similar, adding a component. AGPL-3.0 applies to network use.

**InfluxDB 3 Core** (Rust, MIT/Apache 2.0, GA April 2025): Rewrote in Rust using the FDAP stack (Arrow Flight, DataFusion, Arrow, Parquet). Single binary for Core. Primarily a time-series/metrics engine; logs and traces are not first-class citizens. OTLP data requires external conversion (OTel Collector with InfluxDB exporter or Telegraf OpenTelemetry input plugin — no native OTLP gRPC endpoint). Useful as a storage architecture reference; not an all-in-one observability backend.

**Jaeger v2** (Apache 2.0, CNCF graduated): Traces only. Built on the OTel Collector core, accepting native OTLP. Backed by Cassandra, Elasticsearch, or OpenSearch in production; ClickHouse is available but labeled experimental (behind a feature gate) in current docs. Not a full observability stack.

**Coroot** (Apache 2.0 community tier): Go, eBPF-based auto-instrumentation for metrics, logs, traces, and continuous profiling. Uses ClickHouse for storage — the Coroot binary requires an externally running ClickHouse instance, so labeling it "single binary" is misleading in the same way ClickStack is. Useful for zero-instrumentation Kubernetes shops; not lightweight.

**Netdata** (GPLv3+ agent; dashboard UI under the proprietary NCUL1 license): Per-second metrics with ML anomaly detection, zero-config auto-discovery. OTLP ingest for metrics and logs is GA; trace ingest is in active development with no public timeline. Single agent binary. Strong for infrastructure metrics monitoring; not a distributed-tracing backend.

**Elastic / OpenSearch observability**: Heavy multi-component stacks. Elastic's core code became triple-licensed in September 2024: SSPL 1.0 or AGPLv3 or Elastic License v2 — users choose. OpenSearch remains Apache 2.0. Neither is "lightweight" for self-hosting.

**SaaS reference points** (positioning only): Datadog Pro is approximately $15/host/month plus per-GB for logs and traces. Honeycomb event-based pricing can surge during incidents. Grafana Cloud offers a free tier with consumption-based overage. These define the feature ceiling; soma-observe should not try to reach it.

---

## 3. Comparison Table

| Tool | Signals | Storage Engine | Query Model | OTLP Ingest | Single Binary / Install | License | Language |
|---|---|---|---|---|---|---|---|
| **LGTM Stack** (Prom + Loki + Tempo + Grafana) | Metrics, Logs, Traces | Prometheus TSDB (metrics); Object storage chunks+index (Loki); Object storage Parquet (Tempo) | PromQL + LogQL + TraceQL (three separate languages) | Prometheus 3.0: metrics only; Loki: logs; Tempo: traces; Alloy collector bridges all | No — min. 5 processes, 3 query languages, object storage for Loki+Tempo required | Prometheus: Apache 2.0; Grafana, Loki, Tempo, Mimir: AGPLv3; Alloy: Apache 2.0 | Go |
| **SigNoz** | Metrics, Logs, Traces | ClickHouse (telemetry) + PostgreSQL (metadata) + ClickHouse Keeper (coord.) | Custom visual query builder (generates CH SQL); PromQL for metrics | Yes — gRPC 4317, HTTP 4318, all 3 signals | No — 5-container Docker Compose (app, OTel Collector, CH server, CH Keeper, Postgres); 4 GB RAM minimum | MIT (core); proprietary EE license for `ee/` and `cmd/enterprise/` | Go (37%), TypeScript/React (53%) |
| **Uptrace** | Metrics, Logs, Traces | ClickHouse (telemetry, requires v25.8+ for v2.1.x) + PostgreSQL v14+ (metadata) + Redis (mandatory since v2.0.0) | Custom DSL (spans/logs); partial PromQL-compatible (metrics); no TraceQL | Yes — gRPC 4317, HTTP 4318, all 3 signals | No — 6-service Docker Compose in practice (Uptrace, CH, Postgres, Redis, OTel Collector, MailHog); free Community caps retention at 14 days | AGPL-3.0 (changed from BSL 1.1 at v1.6.0, January 2024); paid tiers for SSO/RBAC | Go (54%), Vue/TypeScript (43%) |
| **OpenObserve** | Logs, Metrics, Traces, RUM, Frontend, LLM | Parquet on S3-compatible object store or local disk; DataFusion query engine; SQLite (single-node) or PostgreSQL (cluster) metadata | SQL (logs/traces), PromQL (metrics, known bugs through late 2025) | Yes — OTLP/HTTP and OTLP/gRPC, all 3 signals | Single binary for single-node (2 env vars, no external deps); HA mode requires NATS + Postgres + object store + multi-role processes | AGPL-3.0 (changed from Apache 2.0 November 2023); proprietary Enterprise for SSO/RBAC/audit/federation | Rust backend (26%), Vue/TypeScript/JS frontend (~70%) |
| **VictoriaMetrics** (metrics) | Metrics | Custom LSM ("mergeset" index, MergeTree-influenced) | MetricsQL (PromQL superset, backwards-compatible) | Yes — OTLP metrics | Single binary, no external deps, one data-path flag | Apache 2.0 | Go |
| **VictoriaLogs** (logs) | Logs | Custom columnar engine (ClickHouse architecture-inspired, daily partitions, immutable parts) | LogsQL (pipeline-style) | Yes — OTLP logs, plus Loki push API, Elasticsearch bulk, journald | Single binary, no external deps | Apache 2.0 | Go |
| **VictoriaTraces** (traces, v0.9.3, pre-GA) | Traces | Same engine as VictoriaLogs (spans stored as structured log events) | LogsQL; Jaeger Query Service JSON API; Tempo API experimental | Yes — OTLP/HTTP and OTLP/gRPC | Single binary, no external deps; native vmui web UI at /select/vmui | Apache 2.0 | Go |
| **Quickwit** | Logs, Traces (no metrics — structural limitation) | Object storage-native (S3/GCS/Azure Blob/MinIO); Tantivy inverted index; local disk for dev | Elasticsearch-compatible REST/DSL + nascent SQL (DataFusion) | Yes — OTLP gRPC for logs and traces; Jaeger gRPC | Single binary via curl install; production requires reachable object storage | Apache 2.0 (relicensed post-Datadog acquisition; Datadog acquired Jan 2025, independent roadmap uncertain) | Rust |
| **ClickStack / HyperDX** | Logs, Metrics, Traces, Session Replay | ClickHouse columnar (MergeTree) | Lucene-style syntax + SQL | Yes — native OTLP gRPC 4317, HTTP 4318 | No — 4 required services: ClickHouse, HyperDX UI, OTel Collector, MongoDB (mandatory for dashboards/alerts); all-in-one Docker image bundles all but is not production-recommended | HyperDX UI: MIT; ClickHouse: Apache 2.0; OTel Collector: Apache 2.0 | TypeScript/Node.js (HyperDX, 94% of UI repo), C++ (ClickHouse), Go (Collector) |
| **GreptimeDB** | Metrics, Logs, Traces (profiles: open issue, not shipped) | Columnar Parquet SST (Flat format default since v1.0); OpenDAL for object store or local disk; Mito2 engine with WAL | SQL + PromQL (no LogQL, no TraceQL) | Yes — OTLP/HTTP all 3 signals; ExponentialHistogram unsupported; delta temporality stored raw | Genuine single binary for standalone (2 commands, no external deps); distributed mode: 3 required processes (Frontend, Datanode, Metasrv) + optional Flownode | Apache 2.0 (core); proprietary GreptimeDB Enterprise License for gated features | Rust |
| **Parseable** | Logs, Metrics, Traces | Parquet on S3-compatible object store; DataFusion query engine | SQL | Yes — native OTLP | Single binary; but local-disk-only deployments still need MinIO or equivalent for production object store | AGPL-3.0 | Rust |
| **InfluxDB 3 Core** | Metrics primarily; logs/traces via external OTel bridge only | Parquet via DataFusion (FDAP stack) on local disk or object store | SQL | No native OTLP endpoint — requires OTel Collector with InfluxDB exporter or Telegraf conversion | Single binary | MIT / Apache 2.0 (GA April 2025) | Rust |
| **Jaeger v2** | Traces only | Cassandra, Elasticsearch, or OpenSearch (stable); ClickHouse (experimental, feature-gated) | Trace search UI; Jaeger Query API | Yes — native OTLP (built on OTel Collector core) | Single binary | Apache 2.0 | Go |

---

## 4. The Standards Layer: OpenTelemetry / OTLP

OpenTelemetry Protocol (OTLP) version 1.10.0 runs over two transports: gRPC (default port 4317) and HTTP/protobuf (port 4318). Both support gzip compression. The data model covers four signals: metrics, logs, and traces are all stable; profiles entered public alpha in early 2026. OpenTelemetry graduated from CNCF on May 21, 2026, with 12,000+ contributors from 2,800+ organizations.

**Why OTLP is the correct ingest contract.** Every major SDK (Go, Java, Python, Rust, Node, .NET) emits OTLP out of the box. AWS CloudWatch, Google Cloud Monitoring/Trace, Azure Monitor, Datadog, and Grafana all accept OTLP natively as of 2026. A backend that speaks OTLP requires zero instrumentation changes for adopters — they point an existing exporter or OTel Collector at the endpoint. Building yet another proprietary agent format would be a step backward and would compete with a standard that has effectively already won.

For soma-observe specifically: the `soma-infra` `http` and `tracing` features provide rustls and a configured tracing subscriber. The OTLP receiver side is a tonic gRPC server consuming `opentelemetry-proto` generated types plus an axum HTTP handler for the HTTP transport — no new plumbing, no reinvented protocol.

**What OTLP does not cover well.** OTLP is push-only. Prometheus's pull model has no OTLP equivalent: Prometheus generates a synthetic `up` metric per scraped target (0 if unreachable), enabling "alert if any target in job X goes silent." A crashed service that stops sending OTLP is indistinguishable from a healthy gap. More practically: the existing Prometheus exporter ecosystem — `node_exporter`, `postgres_exporter`, `blackbox_exporter`, and thousands of others — speaks the Prometheus scrape format or Prometheus remote-write, not OTLP. Bridging it requires an OTel Collector with a Prometheus receiver, which adds an operational component. This is the honest ceiling of an OTLP-only v1.

Prometheus 3.0 (November 2024) added native OTLP ingestion at `/api/v1/otlp/v1/metrics`, but only for metrics. Logs and traces remain outside Prometheus's scope by design.

---

## 5. Honest Verdict: The Unmet Simple Gap

### Where the gap actually is

No existing open-source project delivers a genuine single-binary, one-dependency observability backend covering all three signals without either a heavy external database (ClickHouse, Elasticsearch) or requiring object storage as a hard dependency for local installs.

The closest contenders:

- **OpenObserve** is Rust, single binary, and covers all three signals, but bundles a large Vue SPA and now carries Series A investor expectations that tend to expand feature scope.
- **GreptimeDB** is Rust, genuinely single binary for standalone mode, Apache 2.0, and hit v1.0 GA in April 2026. It is the most direct competitor to take seriously.
- **VictoriaMetrics** is the benchmark for simplicity and resource efficiency but remains three separate binaries, and VictoriaTraces is pre-GA.

The gap soma-observe can credibly own is not "better than all of these at everything." It is: **a Postgres-backed observability ingest layer that integrates naturally into the soma-platform ecosystem without introducing a new stateful dependency**, designed for the subset of teams who are already running Postgres and want observability data in the same storage tier they are already operating.

### The simple-vs-complete tension, stated plainly

"Replace Prometheus + Loki + Tempo + Grafana" is not a v1 description. It is a marketing slogan that deserves scrutiny:

- Replacing Prometheus requires either OTLP-capable instrumentation (the OTel SDK route) or a Prometheus scrape receiver plus remote-write endpoint. An OTLP-only v1 cannot replace Prometheus for users with existing exporter-based infrastructure.
- Replacing Loki at any serious log volume requires log-specific indexing. Plain Postgres with GIN indexes on JSONB handles small to medium log throughput adequately; it is not Loki.
- Replacing Tempo means implementing span storage with trace trees, parent-child links, and sampling-aware querying. That is a meaningfully different schema and query model from flat metrics rows.

A v1 that tries to cover all three signals to production quality in six weeks will do none of them well. A v1 that covers metrics and logs well, with clear documented ceilings, is something a user can evaluate in a weekend and trust in production for small deployments. That is the wedge.

### The phased path

**V1 (target: 6–10 weeks):** OTLP/HTTP ingest for metrics and logs. Plain Postgres storage via the soma-infra connection pool — no new dependencies, no additional `DATABASE_URL`, just new tables under a `soma_observe` schema managed by soma-schema migrations. A small OTel-faithful JSON query API: `GET /api/v1/metrics/query` (time-range metric query with aggregation, preserving OTLP resource/attribute structure) and `GET /api/v1/logs/query` (time-range log query with attribute filter, severity, and body search). OTLP is an ingest-only protocol — OpenTelemetry defines no query or read standard — so the read side is intentionally a custom API; a Prometheus-compatible API would force OTLP data into Prometheus's lossy metric model (flattening resource attributes, losing exponential histogram fidelity, discarding severity and trace correlation). No bundled UI in v1; consumers use curl, an SDK, or any HTTP client. Single Rust binary. Apache 2.0. Document the performance ceiling honestly: approximately 5–10 million metric data points per day and 10 GB of log data before sequential scan query times degrade to user-noticeable levels.

The Postgres schema for metrics uses a normalized model: `metric_series` (one row per unique series, keyed by a stable hash of metric name + resource attributes + datapoint attributes) and `metric_point` (one row per data point: `series_id` FK, `ts`, `value`), range-partitioned by `ts`, with a composite B-tree index on `(series_id, ts)` as the primary access path. Histograms get a dedicated `metric_histogram_point` table with the same partitioning and index, since scalar `value` cannot represent bucket counts. For logs: `body TEXT` with a GIN index on attributes. No TimescaleDB — its useful features (columnar compression, continuous aggregates) are now under the TigerData License (renamed from the Timescale License in June 2025), which is source-available, not open source, and introduces redistribution risk for any project that ships this as part of a redistributable platform component.

**V2 (target: 3–4 months after v1):** Add OTLP/gRPC ingest (tonic). Add a Prometheus remote-write receiver endpoint — this closes the exporter ecosystem gap without requiring users to run an OTel Collector, and it is what makes "replace your existing Prometheus install" a realistic claim rather than an aspiration. Add traces: OTLP spans stored as structured rows in Postgres with `trace_id`, `span_id`, `parent_span_id`, and attributes as `jsonb`, indexed. A `/api/v1/query/traces` endpoint that filters by trace ID and time range, returns all spans in a trace. No TraceQL, no Gantt chart — just the data retrieval layer.

**V3 (when users hit the Postgres ceiling):** Swap the write path to DataFusion + Parquet via soma-infra's `StorageClient` (the `object_store` crate is already present in soma-infra under `storage-s3` and `storage-azure` features). The OTLP ingest API and the HTTP query API remain completely unchanged — this is a pure storage backend swap visible only in configuration (add S3 or local-path environment variables). The ingest contract and the query contract are the stability surface; the storage layer is an implementation detail.

Do not add DataFusion in v1. It has no built-in WAL; compaction must be wired by hand; InfluxDB 3 Core used approximately 30,000 lines of Rust to make the FDAP architecture production-ready. That is not "simple and lightweight."

### The biggest risk

The OTLP-only ingest decision in v1 means soma-observe cannot honestly claim to replace Prometheus for users with existing exporter-based infrastructure. A team running `node_exporter` on three servers pointing at Prometheus still needs either an OTel Collector in front (adding a process) or a minimal Prometheus instance for scraping (defeating the purpose). The correct framing for v1 is: "replace Prometheus for services you instrument with the OTel SDK; add a scrape bridge for legacy exporters in v2." If the proportion of the "small/medium self-hoster" market running legacy exporters is underestimated, v1 adoption will stall because the pitch does not match the actual migration path.

### Why soma-infra Postgres is the right v1 storage choice

The soma-platform already standardizes on Postgres through soma-infra. Every sibling service — soma-vault, soma-iam, soma-audit — already runs against a `DATABASE_URL`. soma-observe is one more consumer of the existing pool and the existing soma-schema migration runner. The install story is "one Postgres, multiple platform services" rather than introducing a second stateful dependency on day one. Operators who already know how to back up, monitor, and scale their platform Postgres do not learn new operational skills to add soma-observe.

The ceiling is real but appropriate: roughly 5–10 million metric data points per day is where sequential scan query performance degrades noticeably. That is approximately 60–115 samples per second of continuous ingest. For "small/medium self-hosters" — the stated target — this covers the vast majority of real deployments. Document the ceiling clearly; do not hide it.

DuckDB is ruled out by its single-writer model, which directly conflicts with concurrent telemetry ingest from multiple services. SQLite WAL mode caps at roughly 80–150K rows per second and adds a second on-disk state store with no operational benefit over Postgres that soma-infra already manages. chDB Rust bindings remain experimental and pull in a large shared library, breaking the single-binary constraint.

---

## 6. Sources

### LGTM Stack (Prometheus / Loki / Tempo / Grafana)

- https://prometheus.io/blog/2024/11/14/prometheus-3-0/ — Prometheus 3.0 release date, native OTLP ingest, Remote Write 2.0
- https://grafana.com/blog/grafana-loki-tempo-relicensing-to-agplv3/ — AGPLv3 relicensing (April 2021)
- https://grafana.com/docs/loki/latest/release-notes/v3-0/ — Loki 3.0 release date, TSDB v13 default
- https://grafana.com/docs/tempo/latest/configuration/hosted-storage/ — Tempo object storage requirement
- https://grafana.com/blog/grafana-agent-to-grafana-alloy-opentelemetry-collector-faq/ — Grafana Agent EOL November 2025, Alloy replacement
- https://grafana.com/docs/mimir/latest/references/architecture/deployment-modes/ — Mimir monolithic mode production caveat
- https://grafana.com/blog/pyroscope-2-0-release/ — Pyroscope 2.0 object storage changes
- https://grafana.com/observability-survey/2025/ — 2025 survey: 39% cite setup/maintenance complexity as top obstacle
- https://grafana.com/blog/announcing-grafana-mimir/ — Mimir launch March 2022, AGPLv3
- https://github.com/grafana/alloy/blob/main/LICENSE — Alloy Apache 2.0 license

### SigNoz

- https://signoz.io/docs/install/docker/ — 5-container compose stack, 4 GB RAM requirement
- https://signoz.io/blog/launching-signoz-single-binary/ — single-binary milestone (v0.76, March 2025)
- https://signoz.io/docs/ingestion/self-hosted/overview/ — OTLP gRPC 4317, HTTP 4318
- https://github.com/SigNoz/signoz/blob/main/LICENSE — MIT core, proprietary EE license
- https://signoz.io/changelog/2026-02-18-introducing-foundry-a-simpler-way-to-deploy-signoz-ub8ipzqlfpwizb79qmp7l80z/ — Foundry introduced v0.112.0

### Uptrace

- https://github.com/uptrace/uptrace/blob/master/CHANGELOG.md — license change BSL to AGPL v1.6.0, v2.0.0 schema rewrite, Redis mandatory
- https://uptrace.dev/get/hosted/install — three required dependencies, 6-service compose
- https://uptrace.dev/editions — Community 14-day retention cap, paid tier feature gates
- https://uptrace.dev/features/querying/metrics — partial PromQL compatibility
- https://uptrace.dev/blog/uptrace-v20 — v2.0.0 release details

### OpenObserve

- https://github.com/openobserve/openobserve — AGPL-3.0 license, language breakdown
- https://openobserve.ai/docs/architecture/ — Parquet/DataFusion storage, deployment modes
- https://openobserve.ai/docs/getting-started/ — single-binary single-node setup
- https://openobserve.ai/blog/what-are-apache-gpl-and-agpl-licenses-and-why-openobserve-moved-from-apache-to-agpl/ — license change November 2023
- https://github.com/openobserve/openobserve/issues/5703 — PromQL arithmetic bug (closed via PR #5719)
- https://github.com/openobserve/openobserve/issues/8777 — PromQL parsing error in v0.15.1 October 2025
- https://www.businesswire.com/news/home/20260429840147/en/OpenObserve-Raises-$10-Million-Series-A-to-Accelerate-AI-native-Observability — $10M Series A April 2026

### VictoriaMetrics Family

- https://docs.victoriametrics.com/victoriametrics/ — single-node architecture, MetricsQL, OTLP metrics
- https://docs.victoriametrics.com/victorialogs/ — VictoriaLogs single binary, LogsQL
- https://docs.victoriametrics.com/victoriatraces/ — VictoriaTraces overview, pre-GA status
- https://docs.victoriametrics.com/victoriatraces/roadmap/ — pre-GA requirements, Tempo API listed as pending
- https://github.com/VictoriaMetrics/VictoriaMetrics — Apache 2.0, v1.146.0 June 22 2026
- https://victoriametrics.com/blog/bsl-is-short-term-fix-why-we-choose-open-source/ — explicit Apache 2.0 commitment, no CLA

### Quickwit

- https://quickwit.io/blog/quickwit-joins-datadog — acquisition announcement January 9 2025
- https://github.com/quickwit-oss/quickwit/blob/main/LICENSE — Apache 2.0 (Datadog, Inc., confirmed live)
- https://github.com/quickwit-oss/quickwit/discussions/3843 — metrics deprioritized, April 2025 maintainer comment
- https://quickwit.io/docs/log-management/otel-service — OTLP gRPC for logs and traces

### HyperDX / ClickStack

- https://www.businesswire.com/news/home/20250313954782/en/ClickHouse-Acquires-HyperDX-to-Accelerate-the-Future-of-Observability — acquisition March 2025
- https://clickhouse.com/clickstack — ClickStack signals, OTLP support, license summary
- https://clickhouse.com/docs/use-cases/observability/clickstack/deployment/docker-compose — 4-service production compose
- https://clickhouse.com/docs/use-cases/observability/clickstack/deployment/all-in-one — all-in-one image: 4 processes in one container, not production-recommended
- https://github.com/ClickHouse/ClickHouse/blob/master/LICENSE — Apache 2.0 (ClickHouse Inc., 2016–2026)
- https://github.com/hyperdxio/hyperdx — MIT license, TypeScript 94%

### GreptimeDB

- https://github.com/GreptimeTeam/greptimedb — Apache 2.0, Rust
- https://docs.greptime.com/getting-started/installation/greptimedb-standalone/ — 2-command install, local disk default
- https://docs.greptime.com/user-guide/ingest-data/for-observability/opentelemetry/ — OTLP endpoints, ExponentialHistogram unsupported, delta temporality caveat
- https://www.greptime.com/blogs/2026-04-14-greptimedb-v1-ga-release — v1.0 GA April 14 2026, Flat SST default
- https://docs.greptime.com/release-notes/release-1-1-1/ — v1.1.1 June 18 2026 (JSON bug fix)
- https://docs.greptime.com/user-guide/concepts/architecture/ — distributed mode: 3 required processes + optional Flownode
- https://www.greptime.com/blogs/2026-06-24-greptimedb-perses-observability — Perses v0.12.0 bundled

### Others

- https://github.com/parseablehq/parseable — Parseable AGPL-3.0, Rust, Parquet/S3
- https://github.com/influxdata/influxdb — InfluxDB 3 Core MIT/Apache 2.0, v3.10.0 June 2026
- https://www.infoq.com/news/2025/04/influxdb3-open-source/ — InfluxDB 3 Core GA April 2025
- https://www.cncf.io/blog/2024/11/12/jaeger-v2-released-opentelemetry-in-the-core/ — Jaeger v2 release, OTLP-native
- https://www.jaegertracing.io/docs/2.dev/storage/ — ClickHouse backend labeled experimental
- https://github.com/coroot/coroot — Apache 2.0, eBPF-based, requires external ClickHouse
- https://www.netdata.cloud/open-source/ — agent GPLv3+, dashboard UI NCUL1 proprietary
- https://www.elastic.co/pricing/faq/licensing — triple license: SSPL / AGPLv3 / ELv2 (AGPLv3 added September 2024)
- https://datafusion.apache.org/blog/2024/11/18/datafusion-fastest-single-node-parquet-clickbench/ — DataFusion 43 fastest single-node Parquet in ClickBench
- https://www.tigerdata.com/docs/about/latest/timescaledb-editions — TimescaleDB Community under TigerData License (source-available)
- https://opentelemetry.io/docs/specs/otlp/ — OTLP spec v1.10.0
- https://www.cncf.io/announcements/2026/05/21/cloud-native-computing-foundation-announces-opentelemetrys-graduation-solidifying-status-as-the-de-facto-observability-standard/ — OTel CNCF graduation May 21 2026
