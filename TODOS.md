# TODOS — soma-observe

## In progress
- **Alerting** (v2 #1) — building now (see v2 roadmap).

## v1 release — publish step (outward-facing, awaiting go)
The release pipeline FILES exist (`Dockerfile`, `docker-compose.yml`, `.github/workflows/ci.yml` + `release.yml`, `install.sh`). **Publishing is not done.** To ship v0.1.0:
1. Repo settings: GHCR `packages: write`, and read access to the sibling path-dep repos (`soma-infra`, `soma-schema`, `soma-ui`) — add a PAT if any are private.
2. `git tag v0.1.0 && git push origin v0.1.0` triggers `release.yml` (binaries + ghcr image + GitHub Release).
3. First CI run only happens on push; watch it and fix any sibling-checkout/permission issues.

## v2 roadmap — close the "basics" gap vs SigNoz (prioritized)
Grounded in the 2026 competitor analysis. The gap is analysis + actionability, not signal coverage.
1. **Alerting** — rule model (metric-threshold + log-count), background evaluator with firing/ok state + `for` duration, webhook notifications (Slack/Discord/PagerDuty/generic), rules CRUD API + an Alerts UI page. *(building)*
2. **Cross-signal correlation + exemplars** — pivot by `trace_id`: trace ↔ logs (logs already store trace_id/span_id), and metric **exemplars** (store a sample trace_id on points) → trace. UI deep-links between the three explorers.
3. **Service map + span-derived RED metrics + histogram percentiles** — derive per-service rate/error/p50/p95/p99 from spans (span-metrics); a dependency graph from parent/child + peer.service; expose histogram quantiles (the deferred percentiles).
4. **Savable custom dashboards** — user-defined panels (metric/log/trace queries) persisted in Postgres + a dashboard builder UI.
5. **Ingest breadth** — OTLP/gRPC (tonic); Prometheus remote-write receiver; `/metrics` scrape compatibility.
6. **Query layer** — a small expression surface (PromQL-subset and/or SQL passthrough) for power users.

## v3 — scale + the eBPF/AI frontier (to challenge Coroot / KubeSense)
- **Storage scale**: DataFusion + Parquet columnar tier + object-storage tiering + downsampling (the documented v3 path; Postgres ceiling ~5-10M points/day).
- **eBPF zero-code auto-instrumentation** — or first-class integration with Grafana Beyla/OBI or the Coroot agent (no-code capture is the KubeSense/Coroot superpower).
- **AI-agentic RCA + anomaly detection** — the 2026 differentiator (KubeSense's "Agentic Data Model"): move from "show data" to "tell the cause."
- **K8s-native** metadata enrichment + topology; **continuous profiling**; **RUM**.
- **Enterprise**: RBAC / SSO / multi-tenancy / audit logs; per-team API tokens (today: one shared bearer).
