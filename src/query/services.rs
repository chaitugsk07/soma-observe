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
pub struct ServiceMapParams {
    /// Start time (RFC3339 or Unix seconds). Default: now-1h.
    pub start: Option<String>,
    /// End time (RFC3339 or Unix seconds). Default: now.
    pub end: Option<String>,
}

// ── Response types ────────────────────────────────────────────────────────────

/// Per-service RED metrics derived from SERVER-kind spans.
#[derive(Debug, Serialize, Deserialize)]
pub struct ServiceStats {
    pub name: String,
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

/// Caller → callee dependency edge.
#[derive(Debug, Serialize, Deserialize)]
pub struct ServiceEdge {
    pub from: String,
    pub to: String,
    pub call_count: i64,
    pub error_count: i64,
    pub p99_ms: f64,
}

/// Response envelope.
#[derive(Debug, Serialize, Deserialize)]
pub struct ServiceMapResponse {
    pub services: Vec<ServiceStats>,
    pub edges: Vec<ServiceEdge>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /api/v1/services?start=&end=
///
/// Returns a service map: per-service RED metrics (from SERVER spans) and
/// cross-service dependency edges (from parent/child span pairs).
///
/// ponytail: single handler, two queries, no abstraction layer — YAGNI.
pub async fn service_map(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ServiceMapParams>,
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

    // ── Query 1: per-service RED from SERVER spans ────────────────────────────
    let service_sql = r#"
        SELECT service_name,
               COUNT(*)                                                   AS span_count,
               COUNT(*) FILTER (WHERE status_code = 'Error')              AS error_count,
               percentile_disc(0.5)  WITHIN GROUP (ORDER BY duration_ns)  AS p50_ns,
               percentile_disc(0.9)  WITHIN GROUP (ORDER BY duration_ns)  AS p90_ns,
               percentile_disc(0.99) WITHIN GROUP (ORDER BY duration_ns)  AS p99_ns
        FROM soma_observe.spans
        WHERE start_time >= $1 AND start_time < $2
          AND kind = 'Server' AND service_name IS NOT NULL
        GROUP BY service_name
        ORDER BY span_count DESC
    "#;

    type ServiceRow = (String, i64, i64, i64, i64, i64);

    let service_rows: Vec<ServiceRow> = match sqlx::query_as(service_sql)
        .bind(start)
        .bind(end)
        .fetch_all(&state.pool)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "service_map services query failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let services: Vec<ServiceStats> = service_rows
        .into_iter()
        .map(|(name, span_count, error_count, p50_ns, p90_ns, p99_ns)| {
            let error_rate = if span_count > 0 {
                error_count as f64 / span_count as f64
            } else {
                0.0
            };
            ServiceStats {
                name,
                span_count,
                error_count,
                error_rate,
                rate_per_sec: span_count as f64 / window_secs,
                p50_ms: p50_ns as f64 / 1_000_000.0,
                p90_ms: p90_ns as f64 / 1_000_000.0,
                p99_ms: p99_ns as f64 / 1_000_000.0,
            }
        })
        .collect();

    // ── Query 2: cross-service edges from parent/child span pairs ─────────────
    //
    // ponytail perf note: self-join on (trace_id, parent_span_id = span_id).
    // Both columns are indexed (trace_id btree, span_id part of the PK), so for
    // moderate trace volumes (<10M rows in window) this is fine. At very high
    // cardinality, a materialized parent_service_name column would avoid the join.
    let edge_sql = r#"
        SELECT parent.service_name                                             AS from_service,
               child.service_name                                              AS to_service,
               COUNT(*)                                                        AS call_count,
               COUNT(*) FILTER (WHERE child.status_code = 'Error')            AS error_count,
               percentile_disc(0.99) WITHIN GROUP (ORDER BY child.duration_ns) AS p99_ns
        FROM soma_observe.spans child
        JOIN soma_observe.spans parent
          ON child.trace_id = parent.trace_id AND child.parent_span_id = parent.span_id
        WHERE child.start_time >= $1 AND child.start_time < $2
          AND parent.service_name IS NOT NULL AND child.service_name IS NOT NULL
          AND parent.service_name <> child.service_name
        GROUP BY parent.service_name, child.service_name
    "#;

    type EdgeRow = (String, String, i64, i64, i64);

    let edge_rows: Vec<EdgeRow> = match sqlx::query_as(edge_sql)
        .bind(start)
        .bind(end)
        .fetch_all(&state.pool)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "service_map edges query failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let edges: Vec<ServiceEdge> = edge_rows
        .into_iter()
        .map(|(from, to, call_count, error_count, p99_ns)| ServiceEdge {
            from,
            to,
            call_count,
            error_count,
            p99_ms: p99_ns as f64 / 1_000_000.0,
        })
        .collect();

    Json(ServiceMapResponse { services, edges }).into_response()
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

    /// Seed a span with a specific kind into the DB.
    #[allow(clippy::too_many_arguments)]
    async fn seed_span(
        pool: &sqlx::PgPool,
        trace_id: &str,
        span_id: &str,
        parent_span_id: Option<&str>,
        service_name: &str,
        kind: &str,
        start_time: chrono::DateTime<Utc>,
        end_time: chrono::DateTime<Utc>,
        status_code: Option<&str>,
    ) {
        let duration_ns = (end_time - start_time)
            .num_nanoseconds()
            .unwrap_or(0)
            .max(0);
        sqlx::query(
            r#"
            INSERT INTO soma_observe.spans
                (trace_id, span_id, parent_span_id, name, service_name, kind,
                 start_time, end_time, duration_ns, status_code, resource, attributes, events, links)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, '{}', '{}', '[]', '[]')
            ON CONFLICT (start_time, trace_id, span_id) DO NOTHING
            "#,
        )
        .bind(trace_id)
        .bind(span_id)
        .bind(parent_span_id)
        .bind(format!("{service_name}-{kind}")) // name = service+kind for readability
        .bind(service_name)
        .bind(kind)
        .bind(start_time)
        .bind(end_time)
        .bind(duration_ns)
        .bind(status_code)
        .execute(pool)
        .await
        .expect("seed span");
    }

    /// Two-service distributed trace: frontend SERVER (root) → frontend CLIENT →
    /// backend SERVER (ok) + a second backend SERVER with Error.
    /// Asserts:
    ///   - services: "frontend" span_count=1, "backend" span_count=2 (error_count=1)
    ///   - edges: frontend→backend call_count >= 1
    #[tokio::test]
    async fn integration_service_map_two_services() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_service_map_two_services: TEST_DATABASE_URL not set");
            return;
        };

        let base = Utc::now() - Duration::minutes(5);
        let suffix = base.timestamp_nanos_opt().unwrap_or(0);
        let trace_id = format!("svcmap{suffix:013x}");

        // frontend SERVER root span (parent_span_id = None)
        seed_span(
            &db.pool,
            &trace_id,
            &format!("{suffix}fe0"),
            None,
            "frontend",
            "Server",
            base,
            base + Duration::milliseconds(200),
            None,
        )
        .await;

        // frontend CLIENT span — child of the SERVER root
        seed_span(
            &db.pool,
            &trace_id,
            &format!("{suffix}fe1"),
            Some(&format!("{suffix}fe0")),
            "frontend",
            "Client",
            base + Duration::milliseconds(10),
            base + Duration::milliseconds(150),
            None,
        )
        .await;

        // backend SERVER span — child of the frontend CLIENT span (cross-service)
        seed_span(
            &db.pool,
            &trace_id,
            &format!("{suffix}be0"),
            Some(&format!("{suffix}fe1")),
            "backend",
            "Server",
            base + Duration::milliseconds(20),
            base + Duration::milliseconds(120),
            None,
        )
        .await;

        // backend SERVER span with Error (second, standalone)
        let trace2 = format!("svcmap2{suffix:012x}");
        seed_span(
            &db.pool,
            &trace2,
            &format!("{suffix}be1"),
            None,
            "backend",
            "Server",
            base + Duration::milliseconds(30),
            base + Duration::milliseconds(80),
            Some("Error"),
        )
        .await;

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));

        let resp = service_map(
            State(state),
            Query(ServiceMapParams {
                start: Some((base - Duration::seconds(1)).to_rfc3339()),
                end: Some((base + Duration::minutes(2)).to_rfc3339()),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let result: ServiceMapResponse = serde_json::from_slice(&body).expect("parse response");

        // ── services assertions ───────────────────────────────────────────────
        let frontend = result
            .services
            .iter()
            .find(|s| s.name == "frontend")
            .expect("frontend must be in services");
        assert_eq!(frontend.span_count, 1, "frontend has 1 SERVER span");
        assert_eq!(frontend.error_count, 0, "frontend has no errors");
        assert_eq!(frontend.error_rate, 0.0);
        assert!(frontend.p50_ms > 0.0, "p50 must be populated");
        assert!(frontend.p90_ms > 0.0, "p90 must be populated");
        assert!(frontend.p99_ms > 0.0, "p99 must be populated");

        let backend = result
            .services
            .iter()
            .find(|s| s.name == "backend")
            .expect("backend must be in services");
        assert_eq!(backend.span_count, 2, "backend has 2 SERVER spans");
        assert_eq!(backend.error_count, 1, "backend has 1 error");
        assert!(backend.error_rate > 0.0 && backend.error_rate <= 1.0);
        assert!(backend.p50_ms > 0.0);
        assert!(backend.p99_ms > 0.0);

        // ── edges assertions ──────────────────────────────────────────────────
        let edge = result
            .edges
            .iter()
            .find(|e| e.from == "frontend" && e.to == "backend")
            .expect("frontend→backend edge must exist");
        assert!(edge.call_count >= 1, "call_count must be >= 1");

        // ── rate_per_sec sanity ───────────────────────────────────────────────
        assert!(frontend.rate_per_sec > 0.0);
        assert!(backend.rate_per_sec > 0.0);
    }

    /// Empty time window → empty services and edges.
    #[tokio::test]
    async fn integration_service_map_empty_window() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_service_map_empty_window: TEST_DATABASE_URL not set");
            return;
        };

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));

        let resp = service_map(
            State(state),
            Query(ServiceMapParams {
                start: Some("2020-01-01T00:00:00Z".to_string()),
                end: Some("2020-01-02T00:00:00Z".to_string()),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let result: ServiceMapResponse = serde_json::from_slice(&body).expect("parse response");
        assert!(result.services.is_empty(), "empty window → no services");
        assert!(result.edges.is_empty(), "empty window → no edges");
    }
}
