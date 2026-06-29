use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{query::{parse_filter, parse_time}, state::AppState};

// ── Query param struct ────────────────────────────────────────────────────────

/// Query params for GET /api/v1/logs/query
#[derive(Debug, Deserialize)]
pub struct LogsQueryParams {
    /// Start time as RFC3339 or Unix timestamp (seconds). Required.
    pub start: String,
    /// End time as RFC3339 or Unix timestamp (seconds). Required.
    pub end: String,
    /// Attribute filter: key="value",key2="value2" (optional).
    pub filter: Option<String>,
    /// Minimum severity number (inclusive). Maps to OTLP severity_number.
    pub severity_min: Option<i32>,
    /// Body substring filter (case-insensitive). Uses ILIKE.
    pub q: Option<String>,
    /// Maximum number of records to return. Default: 100. Max: 1000.
    pub limit: Option<i64>,
}

// ── Response type ─────────────────────────────────────────────────────────────

/// A single log record in the query response.
/// OTel-faithful: preserves resource vs attributes, severity number+text, trace/span ids.
#[derive(Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: i64,
    pub ts: DateTime<Utc>,
    pub severity_number: Option<i32>,
    pub severity_text: Option<String>,
    pub body: Option<String>,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    pub resource: Value,
    pub attributes: Value,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// GET /api/v1/logs/query
///
/// Queries log records with optional filters:
/// - time range (start/end)
/// - attribute jsonb containment filter
/// - minimum severity number
/// - body substring (ILIKE)
/// - result limit
///
/// Returns newline-delimited JSON (one JSON object per line).
pub async fn query_logs(
    State(state): State<Arc<AppState>>,
    Query(params): Query<LogsQueryParams>,
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

    // Clamp limit: default 100, max 1000.
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);

    let attr_filter = parse_filter(params.filter.as_deref());

    // Build the query dynamically based on which optional filters are present.
    // We use $1=start, $2=end, $3=limit as fixed binds, then optionally
    // bind severity_min ($4), body q ($5), and attr_filter ($6).
    //
    // ponytail: build the WHERE clause incrementally rather than a query-builder dep.

    // We'll build a single SQL query with all optional conditions using NULL-passthrough
    // bind pattern: bind the filter value as an Option<T>, and use the form
    //   ($n::type IS NULL OR <condition>)
    // so unused filters are no-ops without dynamic SQL.

    let sql = r#"
        SELECT id, ts, severity_number, severity_text, body, trace_id, span_id, resource, attributes
        FROM soma_observe.logs
        WHERE ts >= $1
          AND ts < $2
          AND ($3::int4 IS NULL OR severity_number >= $3)
          AND ($4::text IS NULL OR body ILIKE '%' || $4 || '%')
          AND ($5::jsonb IS NULL OR attributes @> $5)
        ORDER BY ts DESC
        LIMIT $6
    "#;

    // Type alias keeps the tuple readable and satisfies the clippy::type_complexity lint.
    type LogRow = (
        i64,
        DateTime<Utc>,
        Option<i32>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Value,
        Value,
    );

    let rows: Vec<LogRow> = match sqlx::query_as(sql)
        .bind(start)
        .bind(end)
        .bind(params.severity_min)
        .bind(params.q.as_deref())
        .bind(attr_filter.as_ref())
        .bind(limit)
        .fetch_all(&state.pool)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "logs query failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Serialize as newline-delimited JSON.
    let mut buf = String::new();
    for (id, ts, severity_number, severity_text, body, trace_id, span_id, resource, attributes) in
        rows
    {
        let entry = LogEntry {
            id,
            ts,
            severity_number,
            severity_text,
            body,
            trace_id,
            span_id,
            resource,
            attributes,
        };
        if let Ok(line) = serde_json::to_string(&entry) {
            buf.push_str(&line);
            buf.push('\n');
        }
    }

    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
        buf,
    )
        .into_response()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use serde_json::json;

    // ── Integration tests (require TEST_DATABASE_URL) ─────────────────────────

    /// Returns an isolated TestDb with the schema installed, or None if
    /// TEST_DATABASE_URL is not set. Caller must keep the returned TestDb alive
    /// for the duration of the test so the database is not dropped early.
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
            cors_allow_origin: "*".into(),
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
            alert_eval_interval_secs: 30,
        }
    }

    /// Seed a log record directly into the DB.
    async fn seed_log(
        pool: &sqlx::PgPool,
        ts: DateTime<Utc>,
        severity_number: Option<i32>,
        severity_text: Option<&str>,
        body: Option<&str>,
        resource: &Value,
        attributes: &Value,
    ) {
        sqlx::query(
            r#"
            INSERT INTO soma_observe.logs
                (ts, severity_number, severity_text, body, resource, attributes)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(ts)
        .bind(severity_number)
        .bind(severity_text)
        .bind(body)
        .bind(resource)
        .bind(attributes)
        .execute(pool)
        .await
        .expect("seed log");
    }

    #[tokio::test]
    async fn integration_logs_time_range() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_logs_time_range: TEST_DATABASE_URL not set");
            return;
        };

        // Use a unique run-id so parallel test runs don't contaminate each other.
        let run_id = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let tag = format!("timerange-run-{run_id}");

        let base = Utc::now()
            .checked_sub_signed(Duration::minutes(10))
            .unwrap();

        // Two logs inside the range, one before. All tagged with a unique body prefix.
        seed_log(
            &db.pool,
            base - Duration::minutes(20),
            Some(9),
            Some("INFO"),
            Some(&format!("{tag} old log")),
            &json!({}),
            &json!({}),
        )
        .await;
        seed_log(
            &db.pool,
            base,
            Some(9),
            Some("INFO"),
            Some(&format!("{tag} in-range log 1")),
            &json!({}),
            &json!({}),
        )
        .await;
        seed_log(
            &db.pool,
            base + Duration::minutes(1),
            Some(9),
            Some("INFO"),
            Some(&format!("{tag} in-range log 2")),
            &json!({}),
            &json!({}),
        )
        .await;

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));

        let start_str = (base - Duration::minutes(1)).to_rfc3339();
        let end_str = (base + Duration::minutes(5)).to_rfc3339();

        // Filter by the unique body prefix so we only see this run's logs.
        let resp = query_logs(
            State(state),
            Query(LogsQueryParams {
                start: start_str,
                end: end_str,
                filter: None,
                severity_min: None,
                q: Some(tag.clone()),
                limit: Some(50),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let body_str = String::from_utf8(body_bytes.to_vec()).expect("utf8");
        let lines: Vec<&str> = body_str.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "only two logs are in the time range");
    }

    #[tokio::test]
    async fn integration_logs_severity_filter() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_logs_severity_filter: TEST_DATABASE_URL not set");
            return;
        };

        // Unique run-id isolates this test's logs from parallel runs.
        let run_id = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let tag = format!("sevfilter-{run_id}");
        let attrs = json!({"test_run": &tag});

        let base = Utc::now().checked_sub_signed(Duration::minutes(5)).unwrap();

        seed_log(
            &db.pool,
            base,
            Some(5),
            Some("DEBUG"),
            Some("debug msg"),
            &json!({}),
            &attrs,
        )
        .await;
        seed_log(
            &db.pool,
            base + Duration::seconds(1),
            Some(9),
            Some("INFO"),
            Some("info msg"),
            &json!({}),
            &attrs,
        )
        .await;
        seed_log(
            &db.pool,
            base + Duration::seconds(2),
            Some(17),
            Some("WARN"),
            Some("warn msg"),
            &json!({}),
            &attrs,
        )
        .await;

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));

        let start_str = (base - Duration::seconds(1)).to_rfc3339();
        let end_str = (base + Duration::minutes(2)).to_rfc3339();

        // severity_min=9 (INFO) should exclude DEBUG (5).
        // filter by test_run attribute so we only see this run's logs.
        let filter_str = format!("test_run=\"{tag}\"");
        let resp = query_logs(
            State(state),
            Query(LogsQueryParams {
                start: start_str,
                end: end_str,
                filter: Some(filter_str),
                severity_min: Some(9),
                q: None,
                limit: Some(50),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let body_str = String::from_utf8(body_bytes.to_vec()).expect("utf8");
        let lines: Vec<&str> = body_str.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines.len(),
            2,
            "DEBUG log must be excluded at severity_min=9"
        );

        for line in &lines {
            let entry: LogEntry = serde_json::from_str(line).expect("parse entry");
            assert!(
                entry.severity_number.unwrap_or(0) >= 9,
                "all returned logs must meet severity_min"
            );
        }
    }

    #[tokio::test]
    async fn integration_logs_body_substring() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_logs_body_substring: TEST_DATABASE_URL not set");
            return;
        };

        let base = Utc::now().checked_sub_signed(Duration::minutes(5)).unwrap();

        seed_log(
            &db.pool,
            base,
            Some(9),
            Some("INFO"),
            Some("payment processed"),
            &json!({}),
            &json!({}),
        )
        .await;
        seed_log(
            &db.pool,
            base + Duration::seconds(1),
            Some(9),
            Some("INFO"),
            Some("user login"),
            &json!({}),
            &json!({}),
        )
        .await;
        seed_log(
            &db.pool,
            base + Duration::seconds(2),
            Some(9),
            Some("INFO"),
            Some("Payment FAILED"),
            &json!({}),
            &json!({}),
        )
        .await;

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));

        let start_str = (base - Duration::seconds(1)).to_rfc3339();
        let end_str = (base + Duration::minutes(2)).to_rfc3339();

        // q="payment" should match case-insensitively → 2 matches.
        let resp = query_logs(
            State(state),
            Query(LogsQueryParams {
                start: start_str,
                end: end_str,
                filter: None,
                severity_min: None,
                q: Some("payment".to_string()),
                limit: Some(50),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let body_str = String::from_utf8(body_bytes.to_vec()).expect("utf8");
        let lines: Vec<&str> = body_str.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines.len(),
            2,
            "ILIKE must match 'payment processed' and 'Payment FAILED'"
        );
    }

    #[tokio::test]
    async fn integration_logs_attribute_filter() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_logs_attribute_filter: TEST_DATABASE_URL not set");
            return;
        };

        let base = Utc::now().checked_sub_signed(Duration::minutes(5)).unwrap();

        seed_log(
            &db.pool,
            base,
            Some(9),
            Some("INFO"),
            Some("prod log"),
            &json!({}),
            &json!({"env": "prod"}),
        )
        .await;
        seed_log(
            &db.pool,
            base + Duration::seconds(1),
            Some(9),
            Some("INFO"),
            Some("dev log"),
            &json!({}),
            &json!({"env": "dev"}),
        )
        .await;

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));

        let start_str = (base - Duration::seconds(1)).to_rfc3339();
        let end_str = (base + Duration::minutes(2)).to_rfc3339();

        let resp = query_logs(
            State(state),
            Query(LogsQueryParams {
                start: start_str,
                end: end_str,
                filter: Some(r#"env="prod""#.to_string()),
                severity_min: None,
                q: None,
                limit: Some(50),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let body_str = String::from_utf8(body_bytes.to_vec()).expect("utf8");
        let lines: Vec<&str> = body_str.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 1, "only prod log matches filter");

        let entry: LogEntry = serde_json::from_str(lines[0]).expect("parse");
        assert_eq!(entry.body.as_deref(), Some("prod log"));
    }

    #[tokio::test]
    async fn integration_logs_limit() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_logs_limit: TEST_DATABASE_URL not set");
            return;
        };

        let base = Utc::now().checked_sub_signed(Duration::minutes(5)).unwrap();

        // Seed 5 logs.
        for i in 0..5_i64 {
            seed_log(
                &db.pool,
                base + Duration::seconds(i),
                Some(9),
                Some("INFO"),
                Some(&format!("log {i}")),
                &json!({}),
                &json!({}),
            )
            .await;
        }

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));

        let start_str = (base - Duration::seconds(1)).to_rfc3339();
        let end_str = (base + Duration::minutes(2)).to_rfc3339();

        let resp = query_logs(
            State(state),
            Query(LogsQueryParams {
                start: start_str,
                end: end_str,
                filter: None,
                severity_min: None,
                q: None,
                limit: Some(3),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let body_str = String::from_utf8(body_bytes.to_vec()).expect("utf8");
        let lines: Vec<&str> = body_str.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 3, "limit=3 must cap results at 3");
    }
}
