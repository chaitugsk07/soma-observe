# soma-observe — Roadmap (path to a full observability platform)

Where soma-observe is, and the dependency-ordered path to a SigNoz/KubeSense-class
platform. The **order** matters more than the list — each step is the foundation
for the next.

## Where we are (v1, shipped)
- OTLP/HTTP ingest for all three signals: metrics (gauge/sum/histogram), logs, traces.
- Plain-Postgres storage (normalized series, partitioned tables, partition-drop retention).
- Query APIs + an embedded admin UI: Overview, Metrics (charts + histogram view), Logs, Traces (span waterfall), Retention, Alerts.
- Alerting: background rule evaluator (metric-threshold + log-count) with state machine + webhook notifications.
- CORS for direct browser OTLP; optional bearer auth; single binary + Postgres.

## The target: a SigNoz/KubeSense-class platform — 4 pillars
1. **Agentless eBPF zero-code capture** — telemetry from running pods, no SDK.
2. **Kubernetes-native topology + RED metrics** — a service map mapped onto k8s objects.
3. **AI-agentic root-cause analysis** — agents that *tell you the cause*, not just show data.
4. **Horizontal / columnar scale** — k8s + eBPF volume.

## Build order (each unlocks the next)

1. **Cross-signal correlation + exemplars** — *in progress.*
   Pivot trace ↔ log ↔ metric by `trace_id`; exemplars (metric datapoint → representative trace). The substrate the service map and AI-RCA both stand on; cheap because logs already carry `trace_id`/`span_id`. Without it you have three databases, not a platform.

2. **Service map + span-derived RED metrics + percentiles.**
   Per-service rate/error/p50/p99 derived from spans; a dependency graph from parent/child + `peer.service`. KubeSense's headline view, and the canvas eBPF data and AI-RCA both render onto. Builds on #1.

3. **Kubernetes metadata enrichment + a k8s view.**
   Tag telemetry with pod/namespace/node/deployment/workload (OTel `k8sattributes` processor → index those resource attrs → a k8s topology view). Makes it *Kube*-sense, and is the landing pad for eBPF telemetry (which is already tagged with k8s resource attrs).

4. **eBPF zero-code — by integrating OBI/Beyla, not writing kernel code.**
   Bundle/document Grafana **OBI** (Beyla) or **Odigos** as a DaemonSet → OTLP → soma-observe. Leverage the OTel-eBPF ecosystem (it all emits OTLP, so the backend is unchanged). The eBPF-sourced spans populate the service map (#2) and k8s topology (#3). Building a custom CO-RE/libbpf agent is a separate multi-quarter track, taken only once this proves out.
   - **Parallel hard dependency — columnar storage tier (DataFusion/Parquet).** eBPF + k8s = enormous volume (every request across every pod). The Postgres ceiling (~5-10M points/day) will not hold it, so scale stops being optional here. Start it in parallel with #3/#4. (The OTLP ingest + query contracts stay unchanged; only storage swaps underneath — see install-design.md §8.)

5. **AI-agentic RCA + anomaly detection** — the capstone, and the real differentiator.
   Feed an LLM the correlated telemetry + service map + recent k8s changes and ask "why did latency spike?" → it does exactly the cross-signal pivots built in #1 and proposes a root cause + fix. Only as good as the structured, correlated data beneath it (needs #1-#3). soma-platform already ships `soma-infra::llm` (an Anthropic client) — the LLM plumbing KubeSense had to build is already here. This is the 2026 frontier.

## Supporting / parallel (needed for "full-fledged", off the critical path)
- **Continuous profiling** — OTLP profiles signal + flamegraph UI (the 4th signal).
- **Savable custom dashboards** — user-defined panels persisted in Postgres.
- **Ingest breadth** — OTLP/gRPC (tonic); Prometheus remote-write + scrape.
- **Enterprise** — RBAC / SSO / multi-tenancy / audit logs; per-team API tokens (today: one shared bearer).

## Strategy — where soma-observe actually wins
Becoming a full KubeSense is a multi-year, multi-person, funded effort (Coroot/KubeSense spent years on eBPF + AI). Do **not** try to out-eBPF them by writing kernel code. The edge:
- **OTLP-native + dead-simple** — one binary; plug into the entire OTel/eBPF ecosystem for free (eBPF agents emit OTLP, so they "just work").
- **Embedded in the soma SDLC platform** — observability that ships *with* the dev platform, not bolted on.
- **AI-RCA on day-one LLM infra** (`soma-infra::llm`) — the part of KubeSense that's genuinely new in 2026.

The play: **correlation → service map + RED → k8s topology → eBPF-via-OBI (+ columnar scale) → AI-RCA.**
