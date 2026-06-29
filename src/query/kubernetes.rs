use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::{query::parse_time, state::AppState};

// ── Query params ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct K8sParams {
    /// Start time (RFC3339 or Unix seconds). Default: now-1h.
    pub start: Option<String>,
    /// End time (RFC3339 or Unix seconds). Default: now.
    pub end: Option<String>,
}

// ── Response types ────────────────────────────────────────────────────────────

/// Per-workload RED metrics derived from k8s resource attributes on spans.
#[derive(Debug, Serialize, Deserialize)]
pub struct K8sWorkload {
    pub workload: String,
    pub kind: String, // Deployment | StatefulSet | DaemonSet | Service
    pub pods: Vec<String>,
    pub pod_count: i64,
    pub span_count: i64,
    pub error_count: i64,
    /// error_count / span_count (0.0 when span_count == 0).
    pub error_rate: f64,
    /// span_count / window_seconds.
    pub rate_per_sec: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
}

/// A Kubernetes namespace with its workloads and rolled-up counts.
#[derive(Debug, Serialize, Deserialize)]
pub struct K8sNamespace {
    pub name: String,
    /// Sum of workload pod_counts.
    pub pod_count: i64,
    pub span_count: i64,
    pub error_count: i64,
    pub workloads: Vec<K8sWorkload>,
}

/// Response envelope.
#[derive(Debug, Serialize, Deserialize)]
pub struct K8sTopologyResponse {
    pub namespaces: Vec<K8sNamespace>,
    pub node_count: i64,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// GET /api/v1/kubernetes?start=&end=
///
/// Returns a Kubernetes topology: spans grouped by namespace → workload with
/// RED health metrics. Workload identity is derived entirely from OTLP resource
/// attributes (k8s.namespace.name, k8s.deployment.name, etc.) written by the
/// OTel Collector's k8sattributes processor — no ingest change required.
///
/// ponytail: single handler, two queries, flat→nested fold in Rust — YAGNI.
pub async fn k8s_topology(
    State(state): State<Arc<AppState>>,
    Query(params): Query<K8sParams>,
) -> Response {
    let now = Utc::now();
    let end = match params.end.as_deref().map(parse_time) {
        Some(Some(t)) => t,
        Some(None) => return (StatusCode::BAD_REQUEST, "invalid end time").into_response(),
        None => now,
    };
    let start = match params.start.as_deref().map(parse_time) {
        Some(Some(t)) => t,
        Some(None) => return (StatusCode::BAD_REQUEST, "invalid start time").into_response(),
        None => end - chrono::Duration::hours(1),
    };
    if start >= end {
        return (StatusCode::BAD_REQUEST, "start must be before end").into_response();
    }

    let window_secs = ((end - start).num_seconds().max(1)) as f64;

    // ── Query 1: per-workload RED grouped by namespace+workload ───────────────
    let workload_sql = r#"
        SELECT
            resource->>'k8s.namespace.name' AS namespace,
            COALESCE(
                resource->>'k8s.deployment.name',
                resource->>'k8s.statefulset.name',
                resource->>'k8s.daemonset.name',
                service_name
            ) AS workload,
            CASE
                WHEN resource ? 'k8s.deployment.name'  THEN 'Deployment'
                WHEN resource ? 'k8s.statefulset.name' THEN 'StatefulSet'
                WHEN resource ? 'k8s.daemonset.name'   THEN 'DaemonSet'
                ELSE 'Service'
            END AS wl_kind,
            COUNT(DISTINCT resource->>'k8s.pod.name')                           AS pod_count,
            array_agg(DISTINCT resource->>'k8s.pod.name')
                FILTER (WHERE resource ? 'k8s.pod.name')                        AS pods,
            COUNT(*)                                                             AS span_count,
            COUNT(*) FILTER (WHERE status_code = 'Error')                       AS error_count,
            percentile_disc(0.5)  WITHIN GROUP (ORDER BY duration_ns)           AS p50_ns,
            percentile_disc(0.9)  WITHIN GROUP (ORDER BY duration_ns)           AS p90_ns,
            percentile_disc(0.99) WITHIN GROUP (ORDER BY duration_ns)           AS p99_ns
        FROM soma_observe.spans
        WHERE start_time >= $1 AND start_time < $2
          AND resource ? 'k8s.namespace.name'
        GROUP BY namespace, workload, wl_kind
        ORDER BY namespace, span_count DESC
    "#;

    // ponytail: workload is Option<String> because COALESCE(..., service_name) can
    // still be NULL when a span has k8s.namespace but no workload attr and no
    // service_name (edge case); rows with NULL workload are skipped below.
    type WlRow = (
        Option<String>, // namespace
        Option<String>, // workload
        String,         // kind
        i64,            // pod_count
        Option<Vec<Option<String>>>, // pods (array_agg may include NULLs)
        i64,            // span_count
        i64,            // error_count
        i64,            // p50_ns
        i64,            // p90_ns
        i64,            // p99_ns
    );

    let wl_rows: Vec<WlRow> = match sqlx::query_as(workload_sql)
        .bind(start)
        .bind(end)
        .fetch_all(&state.pool)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "k8s_topology workloads query failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // ── Query 2: distinct node count (infra summary) ──────────────────────────
    let node_sql = r#"
        SELECT COUNT(DISTINCT resource->>'k8s.node.name')
        FROM soma_observe.spans
        WHERE start_time >= $1 AND start_time < $2
          AND resource ? 'k8s.node.name'
    "#;

    let node_count: i64 = match sqlx::query_scalar(node_sql)
        .bind(start)
        .bind(end)
        .fetch_one(&state.pool)
        .await
    {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "k8s_topology node count query failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // ── Fold flat rows → namespace → workload tree ────────────────────────────
    // Use insertion-order-preserving index to keep ORDER BY namespace,span_count.
    let mut ns_index: Vec<String> = Vec::new();
    let mut ns_map: std::collections::HashMap<String, K8sNamespace> =
        std::collections::HashMap::new();

    for (ns_opt, wl_opt, kind, pod_count, pods_opt, span_count, error_count, p50_ns, p90_ns, p99_ns) in wl_rows {
        let Some(ns) = ns_opt else { continue };
        let Some(workload) = wl_opt else { continue };

        let error_rate = if span_count > 0 {
            error_count as f64 / span_count as f64
        } else {
            0.0
        };

        // Flatten Option<Vec<Option<String>>> → Vec<String>, dropping NULLs.
        let pods: Vec<String> = pods_opt
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .collect();

        let wl = K8sWorkload {
            workload,
            kind,
            pods,
            pod_count,
            span_count,
            error_count,
            error_rate,
            rate_per_sec: span_count as f64 / window_secs,
            p50_ms: p50_ns as f64 / 1_000_000.0,
            p90_ms: p90_ns as f64 / 1_000_000.0,
            p99_ms: p99_ns as f64 / 1_000_000.0,
        };

        let entry = ns_map.entry(ns.clone()).or_insert_with(|| {
            ns_index.push(ns.clone());
            K8sNamespace {
                name: ns,
                pod_count: 0,
                span_count: 0,
                error_count: 0,
                workloads: Vec::new(),
            }
        });
        entry.pod_count += pod_count;
        entry.span_count += span_count;
        entry.error_count += error_count;
        entry.workloads.push(wl);
    }

    let namespaces: Vec<K8sNamespace> = ns_index
        .into_iter()
        .filter_map(|ns| ns_map.remove(&ns))
        .collect();

    Json(K8sTopologyResponse {
        namespaces,
        node_count,
    })
    .into_response()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    async fn test_db() -> Option<soma_infra::TestDb> {
        if std::env::var("TEST_DATABASE_URL").is_err() {
            return None;
        }
        let db = soma_infra::TestDb::create_from_env()
            .await
            .expect("create isolated test db");
        crate::install::install(&db.pool)
            .await
            .expect("install schema");
        crate::store::partition::ensure_partitions(&db.pool)
            .await
            .expect("ensure partitions");
        Some(db)
    }

    fn make_cfg() -> crate::config::Config {
        crate::config::Config {
            database_url: std::env::var("TEST_DATABASE_URL").unwrap_or_default(),
            listen_addr: "127.0.0.1:4318".into(),
            auth_token: None,
            metrics_retention_days: 90,
            logs_retention_days: 30,
            traces_retention_days: 7,
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
            cors_allow_origin: "*".into(),
            alert_eval_interval_secs: 30,
        }
    }

    /// Seeds a span with k8s resource attributes.
    #[allow(clippy::too_many_arguments)]
    async fn seed_k8s_span(
        pool: &sqlx::PgPool,
        trace_id: &str,
        span_id: &str,
        service_name: &str,
        namespace: &str,
        deployment: Option<&str>,
        pod: Option<&str>,
        node: Option<&str>,
        start_time: chrono::DateTime<Utc>,
        end_time: chrono::DateTime<Utc>,
        status_code: Option<&str>,
    ) {
        let duration_ns = (end_time - start_time)
            .num_nanoseconds()
            .unwrap_or(0)
            .max(0);

        let mut resource = serde_json::json!({ "k8s.namespace.name": namespace });
        if let Some(d) = deployment {
            resource["k8s.deployment.name"] = serde_json::json!(d);
        }
        if let Some(p) = pod {
            resource["k8s.pod.name"] = serde_json::json!(p);
        }
        if let Some(n) = node {
            resource["k8s.node.name"] = serde_json::json!(n);
        }

        sqlx::query(
            r#"
            INSERT INTO soma_observe.spans
                (trace_id, span_id, parent_span_id, name, service_name, kind,
                 start_time, end_time, duration_ns, status_code, resource, attributes, events, links)
            VALUES ($1, $2, NULL, $3, $4, 'Server', $5, $6, $7, $8, $9, '{}', '[]', '[]')
            ON CONFLICT (start_time, trace_id, span_id) DO NOTHING
            "#,
        )
        .bind(trace_id)
        .bind(span_id)
        .bind(format!("{service_name}-op"))
        .bind(service_name)
        .bind(start_time)
        .bind(end_time)
        .bind(duration_ns)
        .bind(status_code)
        .bind(&resource)
        .execute(pool)
        .await
        .expect("seed k8s span");
    }

    /// Two workloads in the same namespace → grouped under one namespace entry.
    /// One workload has an error span → error_rate > 0.
    #[tokio::test]
    async fn integration_k8s_topology_grouping() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_k8s_topology_grouping: TEST_DATABASE_URL not set");
            return;
        };

        let base = Utc::now() - Duration::minutes(5);
        let suffix = base.timestamp_nanos_opt().unwrap_or(0).abs();

        let trace_a = format!("k8s{suffix:013x}a");
        let trace_b = format!("k8s{suffix:013x}b");
        let trace_c = format!("k8s{suffix:013x}c");

        // api-server: 2 ok spans across 2 pods
        seed_k8s_span(
            &db.pool,
            &trace_a,
            &format!("{suffix}s0"),
            "api",
            "production",
            Some("api-server"),
            Some("api-pod-0"),
            Some("node-1"),
            base,
            base + Duration::milliseconds(100),
            None,
        )
        .await;

        seed_k8s_span(
            &db.pool,
            &trace_b,
            &format!("{suffix}s1"),
            "api",
            "production",
            Some("api-server"),
            Some("api-pod-1"),
            Some("node-1"),
            base + Duration::milliseconds(10),
            base + Duration::milliseconds(150),
            None,
        )
        .await;

        // worker: 1 error span, different deployment, same namespace
        seed_k8s_span(
            &db.pool,
            &trace_c,
            &format!("{suffix}s2"),
            "worker",
            "production",
            Some("worker"),
            Some("worker-pod-0"),
            Some("node-2"),
            base + Duration::milliseconds(20),
            base + Duration::milliseconds(80),
            Some("Error"),
        )
        .await;

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));

        let resp = k8s_topology(
            State(state),
            Query(K8sParams {
                start: Some((base - Duration::seconds(1)).to_rfc3339()),
                end: Some((base + Duration::minutes(2)).to_rfc3339()),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let result: K8sTopologyResponse =
            serde_json::from_slice(&body).expect("parse k8s topology response");

        // One namespace: "production"
        assert_eq!(result.namespaces.len(), 1, "expected 1 namespace");
        let ns = &result.namespaces[0];
        assert_eq!(ns.name, "production");
        assert_eq!(ns.span_count, 3, "3 spans total");
        assert_eq!(ns.error_count, 1, "1 error total");

        // Two workloads: api-server and worker
        assert_eq!(ns.workloads.len(), 2, "expected 2 workloads");

        let api = ns
            .workloads
            .iter()
            .find(|w| w.workload == "api-server")
            .expect("api-server workload");
        assert_eq!(api.kind, "Deployment");
        assert_eq!(api.span_count, 2);
        assert_eq!(api.error_count, 0);
        assert_eq!(api.error_rate, 0.0);
        assert_eq!(api.pod_count, 2, "api-server has 2 distinct pods");

        let worker = ns
            .workloads
            .iter()
            .find(|w| w.workload == "worker")
            .expect("worker workload");
        assert_eq!(worker.kind, "Deployment");
        assert_eq!(worker.span_count, 1);
        assert_eq!(worker.error_count, 1);
        assert!(worker.error_rate > 0.0 && worker.error_rate <= 1.0);

        // node_count: 2 distinct nodes (node-1, node-2)
        assert_eq!(result.node_count, 2, "expected 2 nodes");
    }
}
