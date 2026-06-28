use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{query::{parse_filter, parse_time}, state::AppState};

// ── Query parameter structs ───────────────────────────────────────────────────

/// Query params for GET /api/v1/metrics/query
#[derive(Debug, Deserialize)]
pub struct MetricsQueryParams {
    /// Metric name (required).
    pub name: String,
    /// Start time as RFC3339 or Unix timestamp (seconds).
    pub start: String,
    /// End time as RFC3339 or Unix timestamp (seconds).
    pub end: String,
    /// Bucket step in seconds (e.g. 60 = 1-minute buckets).
    pub step: Option<i64>,
    /// Attribute filter: key="value",key2="value2" (optional).
    pub filter: Option<String>,
    /// Aggregation function: sum|avg|min|max|count. Default: avg.
    pub agg: Option<String>,
}

/// Query params for GET /api/v1/metrics/series
#[derive(Debug, Deserialize)]
pub struct SeriesParams {
    pub name: String,
}

// ── Response types ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct MetricsQueryResponse {
    pub metric: String,
    pub unit: Option<String>,
    pub series: Vec<SeriesResult>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SeriesResult {
    pub series_id: i64,
    pub resource: Value,
    pub attributes: Value,
    pub kind: String,
    pub points: Vec<Point>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Point {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub value: Option<f64>,
    /// Only set for count aggregation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MetricNamesResponse {
    pub names: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MetricSeriesItem {
    pub series_id: i64,
    pub name: String,
    pub kind: String,
    pub unit: Option<String>,
    pub resource: Value,
    pub attributes: Value,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Validate and normalize the aggregation function.
/// Returns lowercase agg name; defaults to "avg".
fn parse_agg(agg: Option<&str>) -> Result<&str, &'static str> {
    match agg.unwrap_or("avg") {
        "sum" => Ok("sum"),
        "avg" => Ok("avg"),
        "min" => Ok("min"),
        "max" => Ok("max"),
        "count" => Ok("count"),
        _ => Err("agg must be one of: sum, avg, min, max, count"),
    }
}

/// Build a Postgres aggregate expression for the given agg function.
fn agg_sql(agg: &str) -> &'static str {
    match agg {
        "sum" => "SUM(mp.value)",
        "avg" => "AVG(mp.value)",
        "min" => "MIN(mp.value)",
        "max" => "MAX(mp.value)",
        "count" => "COUNT(mp.value)",
        _ => "AVG(mp.value)",
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /api/v1/metrics/query
///
/// Aggregates scalar metric points (gauge and counter deltas) over a time range.
/// Groups by series and time bucket (date_bin). Counters return sum of deltas per
/// bucket; gauges return the requested aggregation.
pub async fn query_metrics(
    State(state): State<Arc<AppState>>,
    Query(params): Query<MetricsQueryParams>,
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

    let step_secs = params.step.unwrap_or(60).max(1);

    let agg = match parse_agg(params.agg.as_deref()) {
        Ok(a) => a,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };

    let attr_filter = parse_filter(params.filter.as_deref());

    // Resolve matching series for this metric name (+optional attribute filter).
    // The filter is applied as a jsonb containment check (@>) on attributes.
    struct SeriesRow {
        series_id: i64,
        resource: Value,
        attributes: Value,
        kind: String,
        unit: Option<String>,
    }

    // Build WHERE clause based on whether we have a filter.
    let series_rows: Vec<SeriesRow> = {
        // We always need name match; optionally filter on attributes containment.
        // Using query_as with dynamic SQL avoids an extra crate; we bind the filter
        // as a jsonb parameter so Postgres does the evaluation.
        if let Some(ref af) = attr_filter {
            sqlx::query_as::<_, (i64, Value, Value, String, Option<String>)>(
                r#"
                SELECT series_id, resource, attributes, kind, unit
                FROM soma_observe.metric_series
                WHERE name = $1 AND attributes @> $2::jsonb
                "#,
            )
            .bind(&params.name)
            .bind(af)
            .fetch_all(&state.pool)
            .await
        } else {
            sqlx::query_as::<_, (i64, Value, Value, String, Option<String>)>(
                r#"
                SELECT series_id, resource, attributes, kind, unit
                FROM soma_observe.metric_series
                WHERE name = $1
                "#,
            )
            .bind(&params.name)
            .fetch_all(&state.pool)
            .await
        }
    }
    .unwrap_or_default()
    .into_iter()
    .map(|(series_id, resource, attributes, kind, unit)| SeriesRow {
        series_id,
        resource,
        attributes,
        kind,
        unit,
    })
    .collect();

    if series_rows.is_empty() {
        return Json(MetricsQueryResponse {
            metric: params.name.clone(),
            unit: None,
            series: vec![],
        })
        .into_response();
    }

    // Aggregate metric_point for each series using date_bin.
    // Returns: series_id, bucket_start, aggregated_value, point_count.
    struct BucketRow {
        series_id: i64,
        bucket: DateTime<Utc>,
        value: Option<f64>,
        count: i64,
    }

    // Build aggregate expression; for count, the "value" column holds the count.
    let agg_expr = agg_sql(agg);

    // Use CAST instead of make_interval for compatibility (no type-checked query needed).
    // Bind series_ids as an array for IN check.
    let series_ids: Vec<i64> = series_rows.iter().map(|r| r.series_id).collect();

    // Dynamic SQL: substitute the aggregate expression (it's a fixed-set enum, safe).
    let sql = format!(
        r#"
        SELECT
            mp.series_id,
            date_bin(
                make_interval(secs => $1::float8),
                mp.ts,
                'epoch'::timestamptz
            ) AS bucket,
            {agg_expr} AS agg_value,
            COUNT(*) AS point_count
        FROM soma_observe.metric_point mp
        WHERE mp.series_id = ANY($2::bigint[])
          AND mp.ts >= $3
          AND mp.ts < $4
        GROUP BY mp.series_id, bucket
        ORDER BY mp.series_id, bucket
        "#,
        agg_expr = agg_expr,
    );

    let bucket_rows: Vec<BucketRow> =
        sqlx::query_as::<_, (i64, DateTime<Utc>, Option<f64>, i64)>(&sql)
            .bind(step_secs as f64)
            .bind(&series_ids)
            .bind(start)
            .bind(end)
            .fetch_all(&state.pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(series_id, bucket, value, count)| BucketRow {
                series_id,
                bucket,
                value,
                count,
            })
            .collect();

    // Group bucket rows by series_id.
    let mut series_results: Vec<SeriesResult> = series_rows
        .iter()
        .map(|sr| {
            let points: Vec<Point> = bucket_rows
                .iter()
                .filter(|br| br.series_id == sr.series_id)
                .map(|br| {
                    let bucket_end = br.bucket + chrono::Duration::seconds(step_secs);
                    if agg == "count" {
                        Point {
                            start: br.bucket,
                            end: bucket_end,
                            value: Some(br.count as f64),
                            count: Some(br.count),
                        }
                    } else {
                        Point {
                            start: br.bucket,
                            end: bucket_end,
                            value: br.value,
                            count: None,
                        }
                    }
                })
                .collect();
            SeriesResult {
                series_id: sr.series_id,
                resource: sr.resource.clone(),
                attributes: sr.attributes.clone(),
                kind: sr.kind.clone(),
                points,
            }
        })
        .collect();

    // Filter out series with no points.
    series_results.retain(|sr| !sr.points.is_empty());

    let unit = series_rows.first().and_then(|r| r.unit.clone());

    Json(MetricsQueryResponse {
        metric: params.name.clone(),
        unit,
        series: series_results,
    })
    .into_response()
}

/// GET /api/v1/metrics/names
///
/// Returns all distinct metric names in the series table.
pub async fn list_metric_names(State(state): State<Arc<AppState>>) -> Response {
    let rows: Vec<(String,)> =
        match sqlx::query_as("SELECT DISTINCT name FROM soma_observe.metric_series ORDER BY name")
            .fetch_all(&state.pool)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "list_metric_names query failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };

    let names: Vec<String> = rows.into_iter().map(|(n,)| n).collect();
    Json(MetricNamesResponse { names }).into_response()
}

/// GET /api/v1/metrics/series?name=<name>
///
/// Returns all series (resource + attributes) for the given metric name.
pub async fn list_metric_series(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SeriesParams>,
) -> Response {
    let rows: Vec<(i64, String, String, Option<String>, Value, Value)> = match sqlx::query_as(
        r#"
        SELECT series_id, name, kind, unit, resource, attributes
        FROM soma_observe.metric_series
        WHERE name = $1
        ORDER BY series_id
        "#,
    )
    .bind(&params.name)
    .fetch_all(&state.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "list_metric_series query failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let items: Vec<MetricSeriesItem> = rows
        .into_iter()
        .map(
            |(series_id, name, kind, unit, resource, attributes)| MetricSeriesItem {
                series_id,
                name,
                kind,
                unit,
                resource,
                attributes,
            },
        )
        .collect();

    Json(items).into_response()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use serde_json::json;

    // ── Unit tests (no DB) ────────────────────────────────────────────────────

    #[test]
    fn parse_agg_valid() {
        assert_eq!(parse_agg(Some("sum")), Ok("sum"));
        assert_eq!(parse_agg(Some("avg")), Ok("avg"));
        assert_eq!(parse_agg(None), Ok("avg"));
    }

    #[test]
    fn parse_agg_invalid() {
        assert!(parse_agg(Some("median")).is_err());
    }

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

    /// Insert a series row and return its series_id.
    async fn insert_series(
        pool: &sqlx::PgPool,
        name: &str,
        kind: &str,
        resource: &Value,
        attributes: &Value,
    ) -> i64 {
        use crate::store::schema::{hash_series_key, SeriesKey};
        let key = SeriesKey::new(name, kind, resource, attributes);
        let sid = hash_series_key(&key);
        sqlx::query(
            r#"
            INSERT INTO soma_observe.metric_series (series_id, name, kind, resource, attributes)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (name, kind, resource, attributes) DO NOTHING
            "#,
        )
        .bind(sid)
        .bind(name)
        .bind(kind)
        .bind(resource)
        .bind(attributes)
        .execute(pool)
        .await
        .expect("insert series");
        sid
    }

    /// Insert a scalar metric point.
    async fn insert_point(pool: &sqlx::PgPool, series_id: i64, ts: DateTime<Utc>, value: f64) {
        sqlx::query(
            "INSERT INTO soma_observe.metric_point (series_id, ts, value)
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(series_id)
        .bind(ts)
        .bind(value)
        .execute(pool)
        .await
        .expect("insert point");
    }

    #[tokio::test]
    async fn integration_list_names_discovers_metric() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_list_names_discovers_metric: TEST_DATABASE_URL not set");
            return;
        };

        let unique = format!(
            "discover.{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        insert_series(&db.pool, &unique, "Gauge", &json!({}), &json!({})).await;

        let state = Arc::new(crate::state::AppState::new(
            db.pool.clone(),
            crate::config::Config {
                database_url: std::env::var("TEST_DATABASE_URL").unwrap_or_default(),
                listen_addr: "127.0.0.1:4318".into(),
                auth_token: None,
                metrics_retention_days: 90,
                logs_retention_days: 30,
                ingest_window_secs: 3600,
                future_tolerance_secs: 300,
            },
        ));

        let resp = list_metric_names(State(state)).await;
        let status = resp.status();
        assert_eq!(status, StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let parsed: MetricNamesResponse = serde_json::from_slice(&body).expect("parse");
        assert!(
            parsed.names.contains(&unique),
            "newly inserted metric should appear in names"
        );
    }

    #[tokio::test]
    async fn integration_list_series_for_name() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_list_series_for_name: TEST_DATABASE_URL not set");
            return;
        };

        let metric = format!(
            "series.list.{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let res1 = json!({"host": "a"});
        let res2 = json!({"host": "b"});
        insert_series(&db.pool, &metric, "Gauge", &res1, &json!({})).await;
        insert_series(&db.pool, &metric, "Gauge", &res2, &json!({})).await;

        let cfg = crate::config::Config {
            database_url: std::env::var("TEST_DATABASE_URL").unwrap_or_default(),
            listen_addr: "127.0.0.1:4318".into(),
            auth_token: None,
            metrics_retention_days: 90,
            logs_retention_days: 30,
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
        };
        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), cfg));

        let resp = list_metric_series(
            State(state),
            Query(SeriesParams {
                name: metric.clone(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let items: Vec<MetricSeriesItem> = serde_json::from_slice(&body).expect("parse");
        assert_eq!(items.len(), 2, "two series must be discovered");
        assert!(items.iter().all(|s| s.name == metric));
    }

    #[tokio::test]
    async fn integration_query_gauge_aggregation() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_query_gauge_aggregation: TEST_DATABASE_URL not set");
            return;
        };

        let metric = format!(
            "gauge.agg.{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let sid = insert_series(&db.pool, &metric, "Gauge", &json!({}), &json!({})).await;

        // Align base to a 120-second bucket boundary (date_bin uses epoch as origin)
        // so all three 10s-spaced points always land in the same bucket.
        // Without alignment, base near a boundary (e.g. t=119s within a bucket)
        // can straddle two buckets: [t, t+10s, t+20s] → [t] + [t+10s, t+20s].
        let raw_secs = (Utc::now() - Duration::minutes(5)).timestamp();
        let aligned_secs = raw_secs - raw_secs.rem_euclid(120);
        let base = chrono::DateTime::from_timestamp(aligned_secs, 0).unwrap();
        insert_point(&db.pool, sid, base, 10.0).await;
        insert_point(&db.pool, sid, base + Duration::seconds(10), 20.0).await;
        insert_point(&db.pool, sid, base + Duration::seconds(20), 30.0).await;

        let cfg = crate::config::Config {
            database_url: std::env::var("TEST_DATABASE_URL").unwrap_or_default(),
            listen_addr: "127.0.0.1:4318".into(),
            auth_token: None,
            metrics_retention_days: 90,
            logs_retention_days: 30,
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
        };
        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), cfg));

        let start_str = (base - Duration::seconds(1)).to_rfc3339();
        let end_str = (base + Duration::minutes(5)).to_rfc3339();

        // avg aggregation over 120-second buckets: all 3 points land in the
        // same bucket → (10+20+30)/3 = 20.0
        let resp = query_metrics(
            State(state),
            Query(MetricsQueryParams {
                name: metric.clone(),
                start: start_str.clone(),
                end: end_str.clone(),
                step: Some(120),
                filter: None,
                agg: Some("avg".to_string()),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let parsed: MetricsQueryResponse = serde_json::from_slice(&body).expect("parse");
        assert_eq!(parsed.metric, metric);
        assert!(!parsed.series.is_empty(), "must return at least one series");

        let first_series = &parsed.series[0];
        assert!(
            !first_series.points.is_empty(),
            "must return at least one point"
        );

        // All three points are within 20 seconds, well under the 120-second
        // bucket, so there must be exactly one bucket with avg = 20.
        let bucket_count = first_series.points.len();
        assert_eq!(
            bucket_count, 1,
            "all 3 points must land in the same 120s bucket"
        );

        let avg_val = first_series.points[0].value.unwrap_or(0.0);
        assert!(
            (avg_val - 20.0).abs() < 1e-9,
            "avg of [10,20,30] = 20, got {avg_val}"
        );
    }

    #[tokio::test]
    async fn integration_query_counter_sum_of_deltas() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_query_counter_sum_of_deltas: TEST_DATABASE_URL not set");
            return;
        };

        // Counter points are already stored as deltas (by ingest layer).
        // Query layer sums them per bucket — that gives the rate.
        let metric = format!(
            "counter.sum.{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let sid = insert_series(&db.pool, &metric, "Sum", &json!({}), &json!({})).await;

        // Align base to a 120-second bucket boundary so all three 10s-spaced
        // points always land in the same bucket (same fix as gauge test above).
        let raw_secs = (Utc::now() - Duration::minutes(5)).timestamp();
        let aligned_secs = raw_secs - raw_secs.rem_euclid(120);
        let base = chrono::DateTime::from_timestamp(aligned_secs, 0).unwrap();
        insert_point(&db.pool, sid, base, 5.0).await;
        insert_point(&db.pool, sid, base + Duration::seconds(10), 10.0).await;
        insert_point(&db.pool, sid, base + Duration::seconds(20), 15.0).await;

        let cfg = crate::config::Config {
            database_url: std::env::var("TEST_DATABASE_URL").unwrap_or_default(),
            listen_addr: "127.0.0.1:4318".into(),
            auth_token: None,
            metrics_retention_days: 90,
            logs_retention_days: 30,
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
        };
        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), cfg));

        let start_str = (base - Duration::seconds(1)).to_rfc3339();
        let end_str = (base + Duration::minutes(5)).to_rfc3339();

        // sum aggregation over 120-second buckets: 5+10+15 = 30
        let resp = query_metrics(
            State(state),
            Query(MetricsQueryParams {
                name: metric.clone(),
                start: start_str,
                end: end_str,
                step: Some(120),
                filter: None,
                agg: Some("sum".to_string()),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let parsed: MetricsQueryResponse = serde_json::from_slice(&body).expect("parse");
        assert!(!parsed.series.is_empty());

        assert_eq!(
            parsed.series[0].points.len(),
            1,
            "all 3 points in same 120s bucket"
        );
        let bucket_sum = parsed.series[0].points[0].value.unwrap_or(0.0);
        assert!(
            (bucket_sum - 30.0).abs() < 0.01,
            "sum of deltas [5,10,15] = 30, got {bucket_sum}"
        );
    }

    #[tokio::test]
    async fn integration_query_attribute_filter() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_query_attribute_filter: TEST_DATABASE_URL not set");
            return;
        };

        let metric = format!(
            "filter.test.{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let sid_prod =
            insert_series(&db.pool, &metric, "Gauge", &json!({}), &json!({"env": "prod"})).await;
        let sid_dev =
            insert_series(&db.pool, &metric, "Gauge", &json!({}), &json!({"env": "dev"})).await;

        let base = Utc::now().checked_sub_signed(Duration::minutes(5)).unwrap();
        insert_point(&db.pool, sid_prod, base, 99.0).await;
        insert_point(&db.pool, sid_dev, base, 1.0).await;

        let cfg = crate::config::Config {
            database_url: std::env::var("TEST_DATABASE_URL").unwrap_or_default(),
            listen_addr: "127.0.0.1:4318".into(),
            auth_token: None,
            metrics_retention_days: 90,
            logs_retention_days: 30,
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
        };
        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), cfg));

        let start_str = (base - Duration::seconds(1)).to_rfc3339();
        let end_str = (base + Duration::minutes(2)).to_rfc3339();

        let resp = query_metrics(
            State(state),
            Query(MetricsQueryParams {
                name: metric.clone(),
                start: start_str,
                end: end_str,
                step: Some(60),
                filter: Some(r#"env="prod""#.to_string()),
                agg: Some("avg".to_string()),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let parsed: MetricsQueryResponse = serde_json::from_slice(&body).expect("parse");

        // Only the prod series should match the filter.
        assert_eq!(
            parsed.series.len(),
            1,
            "filter must select only prod series"
        );
        let pt = &parsed.series[0].points[0];
        assert!(
            (pt.value.unwrap_or(0.0) - 99.0).abs() < 0.01,
            "prod series value must be 99"
        );
    }

    #[tokio::test]
    async fn integration_query_no_results() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP integration_query_no_results: TEST_DATABASE_URL not set");
            return;
        };

        let cfg = crate::config::Config {
            database_url: std::env::var("TEST_DATABASE_URL").unwrap_or_default(),
            listen_addr: "127.0.0.1:4318".into(),
            auth_token: None,
            metrics_retention_days: 90,
            logs_retention_days: 30,
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
        };
        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), cfg));

        let resp = query_metrics(
            State(state),
            Query(MetricsQueryParams {
                name: "nonexistent.metric.xyz".to_string(),
                start: "2024-01-01T00:00:00Z".to_string(),
                end: "2024-01-02T00:00:00Z".to_string(),
                step: Some(3600),
                filter: None,
                agg: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let parsed: MetricsQueryResponse = serde_json::from_slice(&body).expect("parse");
        assert!(parsed.series.is_empty(), "no series for unknown metric");
    }
}
