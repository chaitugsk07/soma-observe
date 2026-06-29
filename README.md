# soma-observe

A self-hosted OpenTelemetry observability backend for the soma-platform. It receives metrics, logs, and traces over OTLP/HTTP, stores them in partitioned Postgres tables, and exposes a query API for dashboards and alerting. Designed for single-tenant, low-cardinality workloads where a full Prometheus/Loki stack is overkill.

```sh
docker compose up -d
```

## Docs

- [Install & design overview](docs/install-design.md)
- [Competitive analysis](docs/competitive-analysis.md)

## License

Apache 2.0
