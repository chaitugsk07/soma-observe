use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{query::parse_time, state::AppState};

// ── Query params ──────────────────────────────────────────────────────────────

/// Query params for GET /api/v1/traces/query
#[derive(Debug, Deserialize)]
pub struct TracesQueryParams {
    /// Filter by service_name (optional).
    pub service: Option<String>,
    /// Filter by root span name (optional).
    pub name: Option<String>,
    /// Filter by status: "ok" or "error" (optional).
    pub status: Option<String>,
    /// Minimum trace duration in milliseconds (optional).
    pub min_duration_ms: Option<i64>,
    /// Maximum trace duration in milliseconds (optional).
    pub max_duration_ms: Option<i64>,
    /// Start time (RFC3339 or Unix seconds). Required.
    pub start: String,
    /// End time (RFC3339 or Unix seconds). Required.
    pub end: String,
    /// Maximum number of trace summaries to return. Default: 50. Max: 500.
    pub limit: Option<i64>,
}

// ── Response types ────────────────────────────────────────────────────────────

/// Summary of a single trace (one row per trace_id).
///
/// SQL approach: GROUP BY trace_id to get span_count and time bounds, then join
/// back to get the root span (parent_span_id IS NULL; fallback: earliest start_time).
/// status=error when ANY span in the trace has status_code='Error'.
#[derive(Debug, Serialize, Deserialize)]
pub struct TraceSummary {
    pub trace_id: String,
    pub root_name: String,
    pub root_service: Option<String>,
    pub start_time: DateTime<Utc>,
    /// Total trace duration in milliseconds (max(end_time) - min(start_time)).
    pub duration_ms: i64,
    pub span_count: i64,
    /// "error" if any span has status_code='Error', else "ok".
    pub status: String,
}

/// Full span row returned by GET /api/v1/traces/:trace_id
#[derive(Debug, Serialize, Deserialize)]
pub struct SpanDetail {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub name: String,
    pub kind: Option<String>,
    pub service_name: Option<String>,
    pub scope_name: Option<String>,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub duration_ns: i64,
    pub status_code: Option<String>,
    pub status_message: Option<String>,
    pub resource: Value,
    pub attributes: Value,
    pub events: Value,
    pub links: Value,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /api/v1/traces/query
///
/// Returns trace summaries for all traces with spans in the given time range.
/// Filters: service, root span name, status (ok|error), min/max duration, limit.
///
/// SQL approach: CTE groups spans by trace_id to compute aggregate stats, then
/// LEFT JOINs to the root span (parent_span_id IS NULL fallback earliest start).
pub async fn query_traces(
    State(state): State<Arc<AppState>>,
    Query(params): Query<TracesQueryParams>,
) -> Response {
    let start = match parse_time(&params.start) {
        Some(t) => t,
        None => return (StatusCode::BAD_REQUEST, "invalid start time").into_response(),
    };
    let end = match parse_time(&params.end) {
        Some(t) => t,
        None => return (StatusCode::BAD_REQUEST, "invalid end time").into_response(),
    };
    if start >= end {
        return (StatusCode::BAD_REQUEST, "start must be before end").into_response();
    }

    let limit = params.limit.unwrap_or(50).clamp(1, 500);

    // Build the query:
    //   - agg CTE: per trace_id: span count, min(start), max(end), error flag
    //   - root CTE: DISTINCT ON (trace_id) pick root span (parent_span_id IS NULL),
    //               ORDER BY trace_id, (parent_span_id IS NULL) DESC, start_time ASC
    //   - JOIN + optional filters + ORDER BY start_time DESC
    //
    // Optional filters use the IS NULL passthrough pattern to avoid dynamic SQL.
    let sql = r#"
        WITH agg AS (
            SELECT
                trace_id,
                MIN(start_time)                                                  AS trace_start,
                EXTRACT(EPOCH FROM (MAX(end_time) - MIN(start_time))) * 1000     AS duration_ms,
                COUNT(*)                                                          AS span_count,
                BOOL_OR(status_code = 'Error')                                   AS has_error
            FROM soma_observe.spans
            WHERE start_time >= $1
              AND start_time < $2
            GROUP BY trace_id
        ),
        root AS (
            SELECT DISTINCT ON (s.trace_id)
                s.trace_id,
                s.name        AS root_name,
                s.service_name AS root_service
            FROM soma_observe.spans s
            WHERE s.start_time >= $1
              AND s.start_time < $2
            ORDER BY
                s.trace_id,
                (s.parent_span_id IS NULL) DESC,
                s.start_time ASC
        )
        SELECT
            agg.trace_id,
            root.root_name,
            root.root_service,
            agg.trace_start,
            agg.duration_ms::bigint,
            agg.span_count,
            CASE WHEN agg.has_error THEN 'error' ELSE 'ok' END AS status
        FROM agg
        JOIN root ON root.trace_id = agg.trace_id
        WHERE ($3::text IS NULL OR EXISTS (
                  SELECT 1 FROM soma_observe.spans s2
                  WHERE s2.trace_id = agg.trace_id
                    AND s2.service_name = $3
                    AND s2.start_time >= $1
                    AND s2.start_time < $2
              ))
          AND ($4::text IS NULL OR root.root_name = $4)
          AND ($5::text IS NULL OR (CASE WHEN agg.has_error THEN 'error' ELSE 'ok' END) = $5)
          AND ($6::bigint IS NULL OR agg.duration_ms >= $6)
          AND ($7::bigint IS NULL OR agg.duration_ms <= $7)
        ORDER BY agg.trace_start DESC
        LIMIT $8
    "#;

    type SummaryRow = (String, String, Option<String>, DateTime<Utc>, i64, i64, String);

    let rows: Vec<SummaryRow> = match sqlx::query_as(sql)
        .bind(start)
        .bind(end)
        .bind(params.service.as_deref())
        .bind(params.name.as_deref())
        .bind(params.status.as_deref())
        .bind(params.min_duration_ms)
        .bind(params.max_duration_ms)
        .bind(limit)
        .fetch_all(&state.pool)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "traces query failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let summaries: Vec<TraceSummary> = rows
        .into_iter()
        .map(
            |(trace_id, root_name, root_service, start_time, duration_ms, span_count, status)| {
                TraceSummary {
                    trace_id,
                    root_name,
                    root_service,
                    start_time,
                    duration_ms,
                    span_count,
                    status,
                }
            },
        )
        .collect();

    Json(summaries).into_response()
}

/// GET /api/v1/traces/:trace_id
///
/// Returns all spans belonging to the given trace_id, ordered by start_time ASC.
pub async fn get_trace(
    State(state): State<Arc<AppState>>,
    Path(trace_id): Path<String>,
) -> Response {
    // No time constraint here: scan the whole table via the trace_id btree index.
    let sql = r#"
        SELECT
            trace_id, span_id, parent_span_id, name, kind, service_name, scope_name,
            start_time, end_time, duration_ns, status_code, status_message,
            resource, attributes, events, links
        FROM soma_observe.spans
        WHERE trace_id = $1
        ORDER BY start_time ASC
    "#;

    type SpanRow = (
        String,
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        DateTime<Utc>,
        DateTime<Utc>,
        i64,
        Option<String>,
        Option<String>,
        Value,
        Value,
        Value,
        Value,
    );

    let rows: Vec<SpanRow> = match sqlx::query_as(sql)
        .bind(&trace_id)
        .fetch_all(&state.pool)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "get_trace query failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if rows.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }

    let spans: Vec<SpanDetail> = rows
        .into_iter()
        .map(
            |(
                trace_id,
                span_id,
                parent_span_id,
                name,
                kind,
                service_name,
                scope_name,
                start_time,
                end_time,
                duration_ns,
                status_code,
                status_message,
                resource,
                attributes,
                events,
                links,
            )| SpanDetail {
                trace_id,
                span_id,
                parent_span_id,
                name,
                kind,
                service_name,
                scope_name,
                start_time,
                end_time,
                duration_ns,
                status_code,
                status_message,
                resource,
                attributes,
                events,
                links,
            },
        )
        .collect();

    Json(spans).into_response()
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

    /// Seed a span directly into the DB for query tests.
    #[allow(clippy::too_many_arguments)]
    async fn seed_span(
        pool: &sqlx::PgPool,
        trace_id: &str,
        span_id: &str,
        parent_span_id: Option<&str>,
        name: &str,
        service_name: Option<&str>,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
        status_code: Option<&str>,
    ) {
        let duration_ns = (end_time - start_time)
            .num_nanoseconds()
            .unwrap_or(0)
            .max(0);
        sqlx::query(
            r#"
            INSERT INTO soma_observe.spans
                (trace_id, span_id, parent_span_id, name, service_name,
                 start_time, end_time, duration_ns, status_code, resource, attributes, events, links)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, '{}', '{}', '[]', '[]')
            ON CONFLICT (start_time, trace_id, span_id) DO NOTHING
            "#,
        )
        .bind(trace_id)
        .bind(span_id)
        .bind(parent_span_id)
        .bind(name)
        .bind(service_name)
        .bind(start_time)
        .bind(end_time)
        .bind(duration_ns)
        .bind(status_code)
        .execute(pool)
        .await
        .expect("seed span");
    }

    // ── Integration: ingest_traces via handler ────────────────────────────────

    /// Full round-trip: ingest 3 spans (root + 2 children, one Error), then query.
    #[tokio::test]
    async fn integration_traces_ingest_and_query() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_traces_ingest_and_query: TEST_DATABASE_URL not set");
            return;
        };

        use crate::ingest::otlp_http::ingest_traces;
        use opentelemetry_proto::tonic::{
            collector::trace::v1::ExportTraceServiceRequest,
            common::v1::{AnyValue, InstrumentationScope, KeyValue},
            resource::v1::Resource,
            trace::v1::{ResourceSpans, ScopeSpans, Span as ProtoSpan, Status, status::StatusCode as SC},
        };

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));

        let now_ns = {
            let now = Utc::now();
            (now.timestamp() as u64) * 1_000_000_000 + now.timestamp_subsec_nanos() as u64
        };

        fn kv(key: &str, val: &str) -> KeyValue {
            KeyValue {
                key: key.to_string(),
                value: Some(AnyValue {
                    value: Some(
                        opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                            val.to_string(),
                        ),
                    ),
                }),
            }
        }

        // trace_id: 16 bytes; span_ids: 8 bytes each.
        let trace_id_bytes: Vec<u8> = (1u8..=16).collect();
        let root_span_id: Vec<u8> = vec![0, 0, 0, 0, 0, 0, 0, 1];
        let child1_span_id: Vec<u8> = vec![0, 0, 0, 0, 0, 0, 0, 2];
        let child2_span_id: Vec<u8> = vec![0, 0, 0, 0, 0, 0, 0, 3];

        let req = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(Resource {
                    attributes: vec![kv("service.name", "svc-test")],
                    dropped_attributes_count: 0,
                }),
                scope_spans: vec![ScopeSpans {
                    scope: Some(InstrumentationScope {
                        name: "test-scope".to_string(),
                        ..Default::default()
                    }),
                    spans: vec![
                        // Root span
                        ProtoSpan {
                            trace_id: trace_id_bytes.clone(),
                            span_id: root_span_id.clone(),
                            parent_span_id: vec![],
                            name: "root-op".to_string(),
                            kind: 1, // Internal
                            start_time_unix_nano: now_ns,
                            end_time_unix_nano: now_ns + 100_000_000, // +100ms
                            attributes: vec![kv("env", "test")],
                            status: Some(Status {
                                code: SC::Ok as i32,
                                message: String::new(),
                            }),
                            ..Default::default()
                        },
                        // Child 1 — ok
                        ProtoSpan {
                            trace_id: trace_id_bytes.clone(),
                            span_id: child1_span_id.clone(),
                            parent_span_id: root_span_id.clone(),
                            name: "child-db".to_string(),
                            kind: 3, // Client
                            start_time_unix_nano: now_ns + 10_000_000,
                            end_time_unix_nano: now_ns + 50_000_000,
                            attributes: vec![],
                            status: Some(Status {
                                code: SC::Ok as i32,
                                message: String::new(),
                            }),
                            ..Default::default()
                        },
                        // Child 2 — Error
                        ProtoSpan {
                            trace_id: trace_id_bytes.clone(),
                            span_id: child2_span_id.clone(),
                            parent_span_id: root_span_id.clone(),
                            name: "child-cache".to_string(),
                            kind: 3, // Client
                            start_time_unix_nano: now_ns + 20_000_000,
                            end_time_unix_nano: now_ns + 80_000_000,
                            attributes: vec![],
                            status: Some(Status {
                                code: SC::Error as i32,
                                message: "cache miss".to_string(),
                            }),
                            ..Default::default()
                        },
                    ],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        };

        let body_bytes = serde_json::to_vec(&req).expect("serialize");
        let http_req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/traces")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body_bytes))
            .unwrap();

        let resp = ingest_traces(State(state.clone()), http_req).await;
        assert_eq!(resp.status(), StatusCode::OK, "ingest_traces must succeed");

        // Verify 3 rows written.
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM soma_observe.spans WHERE trace_id = $1")
                .bind("0102030405060708090a0b0c0d0e0f10")
                .fetch_one(&db.pool)
                .await
                .expect("count spans");
        assert_eq!(count.0, 3, "must store 3 spans");

        // Verify root span has no parent.
        let root: (Option<String>, String, i64) = sqlx::query_as(
            "SELECT parent_span_id, service_name, duration_ns FROM soma_observe.spans
             WHERE trace_id = $1 AND span_id = $2",
        )
        .bind("0102030405060708090a0b0c0d0e0f10")
        .bind("0000000000000001")
        .fetch_one(&db.pool)
        .await
        .expect("fetch root span");
        assert!(root.0.is_none(), "root span must have null parent");
        assert_eq!(root.1, "svc-test", "service_name must be set from resource");
        assert_eq!(root.2, 100_000_000, "duration_ns must be 100ms");

        // Verify child2 has Error status.
        let child2: (Option<String>,) = sqlx::query_as(
            "SELECT status_code FROM soma_observe.spans WHERE trace_id=$1 AND span_id=$2",
        )
        .bind("0102030405060708090a0b0c0d0e0f10")
        .bind("0000000000000003")
        .fetch_one(&db.pool)
        .await
        .expect("fetch child2");
        assert_eq!(child2.0.as_deref(), Some("Error"));
    }

    // ── Integration: query_traces summary ────────────────────────────────────

    #[tokio::test]
    async fn integration_traces_query_summary() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_traces_query_summary: TEST_DATABASE_URL not set");
            return;
        };

        let base = Utc::now() - Duration::minutes(5);
        let suffix = base.timestamp_nanos_opt().unwrap_or(0);
        let trace_id = format!("trace{suffix:016x}");

        // Root + child + error child
        seed_span(
            &db.pool,
            &trace_id,
            "span0001",
            None,
            "root-op",
            Some("svc-query"),
            base,
            base + Duration::milliseconds(200),
            None,
        )
        .await;
        seed_span(
            &db.pool,
            &trace_id,
            "span0002",
            Some("span0001"),
            "child-a",
            Some("svc-query"),
            base + Duration::milliseconds(10),
            base + Duration::milliseconds(100),
            None,
        )
        .await;
        seed_span(
            &db.pool,
            &trace_id,
            "span0003",
            Some("span0001"),
            "child-b",
            Some("svc-query"),
            base + Duration::milliseconds(50),
            base + Duration::milliseconds(150),
            Some("Error"),
        )
        .await;

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));

        let resp = query_traces(
            State(state.clone()),
            Query(TracesQueryParams {
                service: Some("svc-query".to_string()),
                name: None,
                status: None,
                min_duration_ms: None,
                max_duration_ms: None,
                start: (base - Duration::seconds(1)).to_rfc3339(),
                end: (base + Duration::minutes(2)).to_rfc3339(),
                limit: Some(10),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let summaries: Vec<TraceSummary> = serde_json::from_slice(&body).expect("parse");

        // Find our trace (there may be others from parallel tests).
        let our = summaries
            .iter()
            .find(|s| s.trace_id == trace_id)
            .expect("our trace must appear");

        assert_eq!(our.span_count, 3, "span_count must be 3");
        assert_eq!(our.status, "error", "status must be error (one Error span)");
        assert_eq!(our.root_name, "root-op", "root_name must be root-op");
        assert!(our.duration_ms >= 200, "duration_ms >= 200ms");
    }

    // ── Integration: filter by service and status=error ───────────────────────

    #[tokio::test]
    async fn integration_traces_filter_service_status() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_traces_filter_service_status: TEST_DATABASE_URL not set");
            return;
        };

        let base = Utc::now() - Duration::minutes(5);
        let suffix = base.timestamp_nanos_opt().unwrap_or(0);
        let trace_ok = format!("traceok{suffix:014x}");
        let trace_err = format!("traceer{suffix:014x}");

        // OK trace
        seed_span(
            &db.pool,
            &trace_ok,
            "spana001",
            None,
            "op-ok",
            Some("svc-filter"),
            base,
            base + Duration::milliseconds(50),
            None,
        )
        .await;
        // Error trace
        seed_span(
            &db.pool,
            &trace_err,
            "spanb001",
            None,
            "op-err",
            Some("svc-filter"),
            base + Duration::milliseconds(5),
            base + Duration::milliseconds(55),
            Some("Error"),
        )
        .await;

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));

        // Filter status=error — only the error trace.
        let resp = query_traces(
            State(state),
            Query(TracesQueryParams {
                service: Some("svc-filter".to_string()),
                name: None,
                status: Some("error".to_string()),
                min_duration_ms: None,
                max_duration_ms: None,
                start: (base - Duration::seconds(1)).to_rfc3339(),
                end: (base + Duration::minutes(2)).to_rfc3339(),
                limit: Some(50),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let summaries: Vec<TraceSummary> = serde_json::from_slice(&body).unwrap();
        let our: Vec<_> = summaries
            .iter()
            .filter(|s| s.trace_id == trace_err || s.trace_id == trace_ok)
            .collect();
        assert_eq!(our.len(), 1, "only error trace must appear");
        assert_eq!(our[0].status, "error");
    }

    // ── Integration: service filter matches any span (not only root) ─────────

    /// Seed a trace: root span is "frontend", child span is "backend".
    /// Querying service=backend must return the trace (summary shows root=frontend).
    /// Querying service=frontend must also return it.
    /// Querying service=nonexistent must return nothing.
    #[tokio::test]
    async fn integration_traces_service_filter_involves_any_span() {
        let Some(db) = test_db().await else {
            eprintln!(
                "SKIP integration_traces_service_filter_involves_any_span: TEST_DATABASE_URL not set"
            );
            return;
        };

        let base = Utc::now() - Duration::minutes(5);
        let suffix = base.timestamp_nanos_opt().unwrap_or(0);
        let trace_id = format!("involve{suffix:012x}");

        // Root span — service "frontend"
        seed_span(
            &db.pool,
            &trace_id,
            "inv0001",
            None,
            "frontend-root",
            Some("frontend"),
            base,
            base + Duration::milliseconds(300),
            None,
        )
        .await;
        // Child span — service "backend"
        seed_span(
            &db.pool,
            &trace_id,
            "inv0002",
            Some("inv0001"),
            "backend-call",
            Some("backend"),
            base + Duration::milliseconds(10),
            base + Duration::milliseconds(200),
            None,
        )
        .await;

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));
        let window_start = (base - Duration::seconds(1)).to_rfc3339();
        let window_end = (base + Duration::minutes(2)).to_rfc3339();

        // Query service=backend → must return the trace; root_service is "frontend".
        let resp = query_traces(
            State(state.clone()),
            Query(TracesQueryParams {
                service: Some("backend".to_string()),
                name: None,
                status: None,
                min_duration_ms: None,
                max_duration_ms: None,
                start: window_start.clone(),
                end: window_end.clone(),
                limit: Some(50),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let summaries: Vec<TraceSummary> = serde_json::from_slice(&body).unwrap();
        let found = summaries.iter().find(|s| s.trace_id == trace_id)
            .expect("service=backend must return the trace that involves backend");
        assert_eq!(
            found.root_service.as_deref(),
            Some("frontend"),
            "summary root_service must still be the root span's service"
        );

        // Query service=frontend → must also return the trace.
        let resp2 = query_traces(
            State(state.clone()),
            Query(TracesQueryParams {
                service: Some("frontend".to_string()),
                name: None,
                status: None,
                min_duration_ms: None,
                max_duration_ms: None,
                start: window_start.clone(),
                end: window_end.clone(),
                limit: Some(50),
            }),
        )
        .await;
        let body2 = axum::body::to_bytes(resp2.into_body(), usize::MAX)
            .await
            .unwrap();
        let summaries2: Vec<TraceSummary> = serde_json::from_slice(&body2).unwrap();
        assert!(
            summaries2.iter().any(|s| s.trace_id == trace_id),
            "service=frontend must also return the trace"
        );

        // Query service=nonexistent → must not return the trace.
        let resp3 = query_traces(
            State(state),
            Query(TracesQueryParams {
                service: Some("nonexistent".to_string()),
                name: None,
                status: None,
                min_duration_ms: None,
                max_duration_ms: None,
                start: window_start,
                end: window_end,
                limit: Some(50),
            }),
        )
        .await;
        let body3 = axum::body::to_bytes(resp3.into_body(), usize::MAX)
            .await
            .unwrap();
        let summaries3: Vec<TraceSummary> = serde_json::from_slice(&body3).unwrap();
        assert!(
            summaries3.iter().all(|s| s.trace_id != trace_id),
            "service=nonexistent must not return the trace"
        );
    }

    // ── Integration: get_trace returns all spans in order ─────────────────────

    #[tokio::test]
    async fn integration_get_trace_returns_spans() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_get_trace_returns_spans: TEST_DATABASE_URL not set");
            return;
        };

        let base = Utc::now() - Duration::minutes(5);
        let suffix = base.timestamp_nanos_opt().unwrap_or(0);
        let trace_id = format!("gettrace{suffix:010x}");

        seed_span(
            &db.pool,
            &trace_id,
            "gs0001",
            None,
            "root",
            Some("svc-get"),
            base,
            base + Duration::milliseconds(300),
            None,
        )
        .await;
        seed_span(
            &db.pool,
            &trace_id,
            "gs0002",
            Some("gs0001"),
            "child-1",
            Some("svc-get"),
            base + Duration::milliseconds(10),
            base + Duration::milliseconds(100),
            None,
        )
        .await;
        seed_span(
            &db.pool,
            &trace_id,
            "gs0003",
            Some("gs0001"),
            "child-2",
            Some("svc-get"),
            base + Duration::milliseconds(50),
            base + Duration::milliseconds(200),
            Some("Error"),
        )
        .await;

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));

        let resp = get_trace(
            State(state),
            Path(trace_id.clone()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let spans: Vec<SpanDetail> = serde_json::from_slice(&body).unwrap();
        assert_eq!(spans.len(), 3, "must return 3 spans");

        // Ordered by start_time ASC: root first.
        assert_eq!(spans[0].name, "root");
        assert!(spans[0].parent_span_id.is_none(), "root has no parent");

        // Children have parent link.
        assert_eq!(spans[1].parent_span_id.as_deref(), Some("gs0001"));
        assert_eq!(spans[2].parent_span_id.as_deref(), Some("gs0001"));

        // Third span has Error status.
        assert_eq!(spans[2].status_code.as_deref(), Some("Error"));
    }

    // ── Integration: empty range → empty result ───────────────────────────────

    #[tokio::test]
    async fn integration_traces_empty_range() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_traces_empty_range: TEST_DATABASE_URL not set");
            return;
        };

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));

        // Query a range in the far past.
        let resp = query_traces(
            State(state),
            Query(TracesQueryParams {
                service: None,
                name: None,
                status: None,
                min_duration_ms: None,
                max_duration_ms: None,
                start: "2020-01-01T00:00:00Z".to_string(),
                end: "2020-01-02T00:00:00Z".to_string(),
                limit: Some(10),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let summaries: Vec<TraceSummary> = serde_json::from_slice(&body).unwrap();
        assert!(summaries.is_empty(), "empty range must return no traces");
    }

    // ── Integration: OPTIONS /v1/traces returns CORS headers ──────────────────

    #[tokio::test]
    async fn integration_cors_preflight() {
        use axum::{body::Body, http::Request};
        use tower::util::ServiceExt;

        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_cors_preflight: TEST_DATABASE_URL not set");
            return;
        };

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));
        let app = crate::build_router(state);

        let req = Request::builder()
            .method("OPTIONS")
            .uri("/v1/traces")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(
            resp.headers().contains_key("access-control-allow-origin"),
            "CORS origin header must be present"
        );
    }
}
