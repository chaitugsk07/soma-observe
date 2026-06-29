# eBPF Zero-Code Instrumentation via OBI

**OBI** (OpenTelemetry eBPF Instrumentation, [docs](https://opentelemetry.io/docs/zero-code/obi/) · [source](https://github.com/open-telemetry/opentelemetry-ebpf-instrumentation)) is the OpenTelemetry project's kernel-level auto-instrumentation agent. It attaches eBPF probes to running processes — no code changes, no restarts, no SDK. It captures HTTP/gRPC/SQL/Redis/Kafka traffic and emits **traces + RED metrics** (rate, errors, duration) as standard OTLP.

[Grafana Beyla](https://grafana.com/docs/beyla/latest/) is OBI's downstream distribution. It has the same OTLP behavior and a published Docker image (`grafana/beyla:latest`). The examples below use Beyla; swap in the upstream OBI image (`otel/opentelemetry-ebpf-instrumentation`) for a vendor-neutral setup.

## Why soma-observe needs no changes

OBI emits OTLP. soma-observe ingests OTLP. Point OBI at soma-observe's OTLP endpoint and all zero-code telemetry lands in the existing **Metrics**, **Traces**, **Services** (service map), and **Kubernetes** views automatically. This is also why eBPF was the natural next step after the service map and k8s topology were built — OBI auto-populates both.

## Quickstart (Docker)

```bash
docker compose -f deploy/obi/docker-compose.yml up --build
```

The compose file brings up soma-observe + Postgres, two plain-HTTP nginx services (`frontend` and `backend`, where frontend proxies every request to backend), a load generator, and a Beyla instance that instruments both. Within ~60 seconds:

- `http://localhost:4318/api/v1/services` → returns `frontend` and `backend`.
- Metrics view → `http.server.request.duration` histograms for both services.
- Traces view → full request traces with the frontend→backend span edge.
- Services view → service map shows the `frontend → backend` dependency.

No code in either nginx service was changed.

## Kubernetes

Apply `deploy/obi/k8s-daemonset.yaml`:

```bash
kubectl apply -f deploy/obi/k8s-daemonset.yaml
```

This creates a DaemonSet in the `obi` namespace. It runs with `hostPID: true` and a privileged securityContext so Beyla can attach eBPF probes to every process on each node. A least-privilege CAP_BPF / CAP_PERFMON / CAP_SYS_PTRACE / CAP_NET_ADMIN alternative is commented out in the file.

A ServiceAccount with a minimal ClusterRole (read pods/replicasets/nodes) is included so Beyla can attach Kubernetes metadata (namespace, workload, node) to every span and metric — the same resource attributes that populate soma-observe's Kubernetes topology view.

Set the OTLP endpoint to match where soma-observe is deployed:

```
OTEL_EXPORTER_OTLP_ENDPOINT=http://soma-observe.<namespace>.svc.cluster.local:4318
```

## Requirements and limits

| Requirement | Detail |
|---|---|
| Kernel | 5.8+ with BTF (`/sys/kernel/btf/vmlinux` must exist). |
| Context propagation | 5.17+ (cross-service edges in the service map). |
| TLS interception | OpenSSL/libssl3 and Go `crypto/tls` only. BoringSSL and rustls are NOT supported. |
| HTTPS proxies | Context propagation breaks at L7 load balancers/proxies — cross-service edges may be incomplete for HTTPS traffic that passes through one. Plain HTTP works end-to-end. |
| Privileges | `hostPID: true` + privileged (or the four CAPs listed above). |

## Auth

If soma-observe runs with `AUTH_TOKEN` set, do **not** use `OTEL_EXPORTER_OTLP_HEADERS` — Beyla's metrics exporter ignores it in some versions. Set the per-signal headers instead:

```
OTEL_EXPORTER_OTLP_TRACES_HEADERS=Authorization=Bearer <token>
OTEL_EXPORTER_OTLP_METRICS_HEADERS=Authorization=Bearer <token>
```

## Roadmap note

OBI is the **integration track**: leverage the existing eBPF ecosystem, no kernel code written. A future **soma-probe** (a Rust/Aya-based native eBPF agent) is planned as a separate component that will also emit OTLP — so it swaps in with no backend change. The **columnar storage tier** (DataFusion/Parquet) remains the open scale gate for high-volume eBPF traffic.
