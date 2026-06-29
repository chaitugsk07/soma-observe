# Kubernetes Topology View

## What it shows

The Kubernetes page groups span telemetry by Kubernetes resource attributes and
shows per-workload RED health (Rate, Error %, p50/p99 latency). The hierarchy is:

```
Namespace
  └─ Workload (Deployment | StatefulSet | DaemonSet | Service)
       └─ Pods (distinct pod names observed in the time window)
```

Clicking a workload name pivots to the Traces view filtered by that workload
(best-effort match — works when the workload name equals the OTLP `service.name`).

## How enrichment works

soma-observe does **not** do Kubernetes enrichment itself. It is a pure OTLP
receiver: it stores whatever resource attributes arrive with each span. The
enrichment is done by the OpenTelemetry Collector's `k8sattributes` processor,
which runs in your cluster, watches the Kubernetes API, and tags every span with
the pod/namespace/node/deployment that produced it.

This is an intentional design choice: the backend stays a simple OTLP sink; the
Collector ecosystem handles the k8s-specific intelligence. It also means that
eBPF telemetry agents (Beyla, Odigos, etc.) that already tag their OTLP output
with k8s resource attributes will automatically appear in this view.

## Collector configuration

Add the `k8sattributes` processor to your Collector config and include it in
every pipeline that feeds soma-observe:

```yaml
processors:
  k8sattributes:
    auth_type: serviceAccount
    extract:
      metadata:
        - k8s.namespace.name
        - k8s.pod.name
        - k8s.deployment.name
        - k8s.statefulset.name
        - k8s.daemonset.name
        - k8s.node.name

service:
  pipelines:
    traces:  { processors: [k8sattributes, batch] }
    metrics: { processors: [k8sattributes, batch] }
    logs:    { processors: [k8sattributes, batch] }
```

The Collector must run with a `serviceAccount` that has `get`/`list`/`watch`
access to `pods` (and optionally `nodes`, `replicasets`, `deployments`, etc.)
in the namespaces you want to observe. See the [OpenTelemetry k8sattributes
processor docs](https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/main/processor/k8sattributesprocessor)
for full RBAC and filter configuration.

## Resource attribute keys

The view reads these keys from `spans.resource` (a JSONB column):

| Attribute key              | Used for                         |
|----------------------------|----------------------------------|
| `k8s.namespace.name`       | Namespace grouping (required)    |
| `k8s.deployment.name`      | Workload name + kind=Deployment  |
| `k8s.statefulset.name`     | Workload name + kind=StatefulSet |
| `k8s.daemonset.name`       | Workload name + kind=DaemonSet   |
| `k8s.pod.name`             | Pod list and pod count           |
| `k8s.node.name`            | Node count (infra summary)       |

Workload kind priority: Deployment → StatefulSet → DaemonSet → Service (falls
back to `service.name` when none of the above are present).

Only spans with `k8s.namespace.name` appear in the Kubernetes view; spans
without it are visible in the Services and Traces views instead.
