# TODOS — soma-observe

Full path to a SigNoz/KubeSense-class platform: **docs/roadmap.md** (dependency-ordered).

## Done

- v1 core: OTLP metrics+logs+traces, Postgres storage, embedded admin UI.
- Histogram query + view.
- Release pipeline FILES (publish pending — see below).
- Alerting: rule evaluator + webhook + Alerts UI.
- **Cross-signal correlation + exemplars** (roadmap #1) — trace ↔ log pivot (deep-links) + metric→trace exemplars. Backend + UI shipped.
- **Service map + span-derived RED metrics + percentiles** (roadmap #2) — per-service RED + dependency graph + service→traces pivot. Backend + UI shipped.
- **Kubernetes topology view** (roadmap #3) — namespace→workload RED from k8s resource attrs. Backend + UI shipped.
- **eBPF zero-code via OBI** (roadmap #4) — docker-compose + k8s DaemonSet deploy recipes + docs/ebpf-obi.md; OTLP-native, no backend change. Columnar storage tier still the open scale gate.

## v1 release — publish step (outward-facing, awaiting go)

The release files exist (`Dockerfile`, `docker-compose.yml`, `.github/workflows/{ci,release}.yml`, `install.sh`). To ship v0.1.0:

1. Repo settings: GHCR `packages: write`; read access to sibling path-dep repos (`soma-infra`, `soma-schema`, `soma-ui`) — PAT if any are private.
2. `git tag v0.1.0 && git push origin v0.1.0` triggers `release.yml`.
3. First CI run happens on push; watch and fix sibling-checkout/permission issues.

## Next — rationale + sequencing in docs/roadmap.md

- **#4 open scale gate** — columnar storage tier (DataFusion/Parquet); the Postgres ceiling won't hold eBPF volume.
- **#5** AI-agentic RCA + anomaly detection (on `soma-infra::llm`).

Supporting / parallel: continuous profiling; savable dashboards; OTLP/gRPC + Prometheus ingest; RBAC/SSO/multi-tenancy.
