# TODOS — soma-observe

Full path to a SigNoz/KubeSense-class platform: **docs/roadmap.md** (dependency-ordered).

## In progress

- **Cross-signal correlation + exemplars** (roadmap #1) — trace ↔ log pivot (deep-links) + metric→trace exemplars. The substrate the service map and AI-RCA both stand on.

## Done

- v1 core: OTLP metrics+logs+traces, Postgres storage, embedded admin UI.
- Histogram query + view.
- Release pipeline FILES (publish pending — see below).
- Alerting: rule evaluator + webhook + Alerts UI.

## v1 release — publish step (outward-facing, awaiting go)

The release files exist (`Dockerfile`, `docker-compose.yml`, `.github/workflows/{ci,release}.yml`, `install.sh`). To ship v0.1.0:

1. Repo settings: GHCR `packages: write`; read access to sibling path-dep repos (`soma-infra`, `soma-schema`, `soma-ui`) — PAT if any are private.
2. `git tag v0.1.0 && git push origin v0.1.0` triggers `release.yml`.
3. First CI run happens on push; watch and fix sibling-checkout/permission issues.

## Next (after correlation) — rationale + sequencing in docs/roadmap.md

- **#2** Service map + span-derived RED metrics + percentiles.
- **#3** Kubernetes metadata enrichment + a k8s topology view.
- **#4** eBPF zero-code via OBI/Beyla integration — **+ columnar storage tier (the scale gate)**.
- **#5** AI-agentic RCA + anomaly detection (on `soma-infra::llm`).

Supporting / parallel: continuous profiling; savable dashboards; OTLP/gRPC + Prometheus ingest; RBAC/SSO/multi-tenancy.
