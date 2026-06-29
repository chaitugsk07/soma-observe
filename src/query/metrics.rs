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
    /// Populated only for Histogram series: bounds + latest bucket counts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub histogram: Option<HistogramSummary>,
}

/// Snapshot of the most recent histogram bucket distribution in range.
/// `bounds` has N entries (explicit upper bounds); `latest_bucket_counts`
/// has N+1 entries (the last is the overflow / +Inf bucket).
#[derive(Debug, Serialize, Deserialize)]
pub struct HistogramSummary {
    pub bounds: Vec<f64>,
    pub latest_bucket_counts: Vec<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Point {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub value: Option<f64>,
    /// Only set for count aggregation (scalar) or histogram series.
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

    // Partition series into scalar (Gauge/Sum) and Histogram.
    let scalar_ids: Vec<i64> = series_rows
        .iter()
        .filter(|r| r.kind != "Histogram")
        .map(|r| r.series_id)
        .collect();
    let histogram_ids: Vec<i64> = series_rows
        .iter()
        .filter(|r| r.kind == "Histogram")
        .map(|r| r.series_id)
        .collect();

    // ── Scalar bucket rows ────────────────────────────────────────────────────

    struct BucketRow {
        series_id: i64,
        bucket: DateTime<Utc>,
        value: Option<f64>,
        count: i64,
    }

    // Build aggregate expression; for count, the "value" column holds the count.
    let agg_expr = agg_sql(agg);

    // Dynamic SQL: substitute the aggregate expression (it's a fixed-set enum, safe).
    let scalar_bucket_rows: Vec<BucketRow> = if scalar_ids.is_empty() {
        vec![]
    } else {
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
        sqlx::query_as::<_, (i64, DateTime<Utc>, Option<f64>, i64)>(&sql)
            .bind(step_secs as f64)
            .bind(&scalar_ids)
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
            .collect()
    };

    // ── Histogram bucket rows (SUM(sum) / SUM(count) per date_bin) ────────────

    struct HistoBucketRow {
        series_id: i64,
        bucket: DateTime<Utc>,
        sum: Option<f64>,
        count: Option<i64>,
    }

    let histo_bucket_rows: Vec<HistoBucketRow> = if histogram_ids.is_empty() {
        vec![]
    } else {
        sqlx::query_as::<_, (i64, DateTime<Utc>, Option<f64>, Option<i64>)>(
            r#"
            SELECT
                mhp.series_id,
                date_bin(
                    make_interval(secs => $1::float8),
                    mhp.ts,
                    'epoch'::timestamptz
                ) AS bucket,
                SUM(mhp.sum)          AS agg_sum,
                SUM(mhp.count)::bigint AS agg_count
            FROM soma_observe.metric_histogram_point mhp
            WHERE mhp.series_id = ANY($2::bigint[])
              AND mhp.ts >= $3
              AND mhp.ts < $4
            GROUP BY mhp.series_id, bucket
            ORDER BY mhp.series_id, bucket
            "#,
        )
        .bind(step_secs as f64)
        .bind(&histogram_ids)
        .bind(start)
        .bind(end)
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(series_id, bucket, sum, count)| HistoBucketRow {
            series_id,
            bucket,
            sum,
            count,
        })
        .collect()
    };

    // ── Latest bucket distribution (most recent histogram point in range) ─────

    struct HistoSummaryRow {
        series_id: i64,
        bounds: Value,
        bucket_counts: Value,
    }

    let histo_summary_rows: Vec<HistoSummaryRow> = if histogram_ids.is_empty() {
        vec![]
    } else {
        // One row per series_id: the latest histogram point in the query range.
        sqlx::query_as::<_, (i64, Value, Value)>(
            r#"
            SELECT DISTINCT ON (mhp.series_id)
                mhp.series_id,
                mhp.bounds,
                mhp.bucket_counts
            FROM soma_observe.metric_histogram_point mhp
            WHERE mhp.series_id = ANY($1::bigint[])
              AND mhp.ts >= $2
              AND mhp.ts < $3
              AND mhp.bounds IS NOT NULL
              AND mhp.bucket_counts IS NOT NULL
            ORDER BY mhp.series_id, mhp.ts DESC
            "#,
        )
        .bind(&histogram_ids)
        .bind(start)
        .bind(end)
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(series_id, bounds, bucket_counts)| HistoSummaryRow {
            series_id,
            bounds,
            bucket_counts,
        })
        .collect()
    };

    // ── Assemble results ──────────────────────────────────────────────────────

    let mut series_results: Vec<SeriesResult> = series_rows
        .iter()
        .map(|sr| {
            if sr.kind == "Histogram" {
                // Histogram points: value = SUM(sum), count = SUM(count)
                let points: Vec<Point> = histo_bucket_rows
                    .iter()
                    .filter(|br| br.series_id == sr.series_id)
                    .map(|br| {
                        let bucket_end = br.bucket + chrono::Duration::seconds(step_secs);
                        Point {
                            start: br.bucket,
                            end: bucket_end,
                            value: br.sum,
                            count: br.count,
                        }
                    })
                    .collect();

                // Build HistogramSummary from the latest bucket distribution.
                let histogram = histo_summary_rows
                    .iter()
                    .find(|hs| hs.series_id == sr.series_id)
                    .and_then(|hs| {
                        let bounds: Vec<f64> = hs
                            .bounds
                            .as_array()?
                            .iter()
                            .filter_map(|v| v.as_f64())
                            .collect();
                        let latest_bucket_counts: Vec<i64> = hs
                            .bucket_counts
                            .as_array()?
                            .iter()
                            .filter_map(|v| v.as_i64())
                            .collect();
                        Some(HistogramSummary {
                            bounds,
                            latest_bucket_counts,
                        })
                    });

                SeriesResult {
                    series_id: sr.series_id,
                    resource: sr.resource.clone(),
                    attributes: sr.attributes.clone(),
                    kind: sr.kind.clone(),
                    points,
                    histogram,
                }
            } else {
                // Scalar series
                let points: Vec<Point> = scalar_bucket_rows
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
                    histogram: None,
                }
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
            traces_retention_days: 7,
            cors_allow_origin: "*".into(),
                ingest_window_secs: 3600,
                future_tolerance_secs: 300,
                alert_eval_interval_secs: 30,
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
            traces_retention_days: 7,
            cors_allow_origin: "*".into(),
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
            alert_eval_interval_secs: 30,
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
            traces_retention_days: 7,
            cors_allow_origin: "*".into(),
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
            alert_eval_interval_secs: 30,
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
            traces_retention_days: 7,
            cors_allow_origin: "*".into(),
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
            alert_eval_interval_secs: 30,
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
            traces_retention_days: 7,
            cors_allow_origin: "*".into(),
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
            alert_eval_interval_secs: 30,
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
            traces_retention_days: 7,
            cors_allow_origin: "*".into(),
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
            alert_eval_interval_secs: 30,
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

    // ── Helper: insert a histogram series + points ────────────────────────────

    /// Insert a Histogram series row and return its series_id.
    async fn insert_histogram_series(pool: &sqlx::PgPool, name: &str) -> i64 {
        insert_series(pool, name, "Histogram", &json!({}), &json!({})).await
    }

    /// Insert a histogram point.
    async fn insert_histogram_point(
        pool: &sqlx::PgPool,
        series_id: i64,
        ts: DateTime<Utc>,
        sum: f64,
        count: i64,
        bounds: &[f64],
        bucket_counts: &[i64],
    ) {
        use crate::store::schema::HistogramPoint;
        use crate::store::write::write_histogram_points;
        write_histogram_points(
            pool,
            &[HistogramPoint {
                series_id,
                ts,
                sum: Some(sum),
                count: Some(count),
                bounds: Some(json!(bounds)),
                bucket_counts: Some(json!(bucket_counts)),
            }],
        )
        .await
        .expect("insert histogram point");
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

    // ── Histogram integration tests ───────────────────────────────────────────

    /// Points for a Histogram series return value=SUM(sum) and count=SUM(count)
    /// per date_bin bucket, and the HistogramSummary is populated with the
    /// latest bucket distribution.
    #[tokio::test]
    async fn integration_query_histogram_points_and_summary() {
        let Some(db) = test_db().await else {
            eprintln!(
                "SKIP integration_query_histogram_points_and_summary: TEST_DATABASE_URL not set"
            );
            return;
        };

        let metric = format!(
            "histo.query.{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let sid = insert_histogram_series(&db.pool, &metric).await;

        // Align base to a 120-second bucket boundary so all points land in the
        // same bucket (same guard as the scalar tests above).
        let raw_secs = (Utc::now() - Duration::minutes(5)).timestamp();
        let aligned_secs = raw_secs - raw_secs.rem_euclid(120);
        let base = chrono::DateTime::from_timestamp(aligned_secs, 0).unwrap();

        // Two histogram observations in the same bucket:
        //   obs1: sum=10, count=3, bounds=[0.5,1.0], bucket_counts=[1,1,1]
        //   obs2: sum=20, count=5, bounds=[0.5,1.0], bucket_counts=[2,2,1]
        // Expected bucket aggregation: value=SUM(sum)=30, count=SUM(count)=8
        insert_histogram_point(
            &db.pool,
            sid,
            base,
            10.0,
            3,
            &[0.5, 1.0],
            &[1, 1, 1],
        )
        .await;
        insert_histogram_point(
            &db.pool,
            sid,
            base + Duration::seconds(10),
            20.0,
            5,
            &[0.5, 1.0],
            &[2, 2, 1],
        )
        .await;

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));
        let start_str = (base - Duration::seconds(1)).to_rfc3339();
        let end_str = (base + Duration::minutes(5)).to_rfc3339();

        let resp = query_metrics(
            State(state),
            Query(MetricsQueryParams {
                name: metric.clone(),
                start: start_str,
                end: end_str,
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

        assert_eq!(parsed.series.len(), 1, "must return one histogram series");
        let sr = &parsed.series[0];
        assert_eq!(sr.kind, "Histogram");

        // One bucket; value = SUM(sum), count = SUM(count)
        assert_eq!(sr.points.len(), 1, "one bucket in range");
        let pt = &sr.points[0];
        assert!(
            (pt.value.unwrap_or(0.0) - 30.0).abs() < 1e-9,
            "SUM(sum) must be 30, got {:?}",
            pt.value
        );
        assert_eq!(pt.count, Some(8), "SUM(count) must be 8");

        // HistogramSummary must be present with the latest observation's data.
        let hs = sr
            .histogram
            .as_ref()
            .expect("histogram summary must be present");
        assert_eq!(hs.bounds, vec![0.5, 1.0], "bounds must match");
        // N bounds → N+1 bucket_counts (overflow included)
        assert_eq!(
            hs.latest_bucket_counts.len(),
            hs.bounds.len() + 1,
            "latest_bucket_counts must have bounds.len()+1 entries (overflow bucket)"
        );
        // Latest obs has bucket_counts=[2,2,1]
        assert_eq!(
            hs.latest_bucket_counts,
            vec![2, 2, 1],
            "latest_bucket_counts must reflect most recent observation"
        );
    }

    /// An empty time range yields empty points and None histogram summary.
    #[tokio::test]
    async fn integration_query_histogram_empty_range() {
        let Some(db) = test_db().await else {
            eprintln!(
                "SKIP integration_query_histogram_empty_range: TEST_DATABASE_URL not set"
            );
            return;
        };

        let metric = format!(
            "histo.empty.{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let sid = insert_histogram_series(&db.pool, &metric).await;

        // Insert a point in the past (outside the query range below).
        let past = Utc::now() - Duration::hours(2);
        insert_histogram_point(&db.pool, sid, past, 5.0, 2, &[1.0], &[1, 1]).await;

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));

        // Query a range that contains no points (one hour ago → 30 min ago).
        let range_start = (Utc::now() - Duration::hours(1)).to_rfc3339();
        let range_end = (Utc::now() - Duration::minutes(30)).to_rfc3339();

        let resp = query_metrics(
            State(state),
            Query(MetricsQueryParams {
                name: metric.clone(),
                start: range_start,
                end: range_end,
                step: Some(60),
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

        // The series exists but has no points in range → retained only if
        // series_results.retain(!points.is_empty()) passes.
        // With no points the series is filtered out.
        assert!(
            parsed.series.is_empty(),
            "no points in range → series filtered out"
        );
    }

    /// Scalar (non-histogram) series must be unaffected by histogram logic:
    /// no `histogram` field in the JSON response (serde skip_serializing_if).
    #[tokio::test]
    async fn integration_scalar_series_has_no_histogram_field() {
        let Some(db) = test_db().await else {
            eprintln!(
                "SKIP integration_scalar_series_has_no_histogram_field: TEST_DATABASE_URL not set"
            );
            return;
        };

        let metric = format!(
            "scalar.nohisto.{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let sid = insert_series(&db.pool, &metric, "Gauge", &json!({}), &json!({})).await;

        let raw_secs = (Utc::now() - Duration::minutes(5)).timestamp();
        let aligned_secs = raw_secs - raw_secs.rem_euclid(120);
        let base = chrono::DateTime::from_timestamp(aligned_secs, 0).unwrap();
        insert_point(&db.pool, sid, base, 7.0).await;

        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), make_cfg()));
        let start_str = (base - Duration::seconds(1)).to_rfc3339();
        let end_str = (base + Duration::minutes(5)).to_rfc3339();

        let resp = query_metrics(
            State(state),
            Query(MetricsQueryParams {
                name: metric.clone(),
                start: start_str,
                end: end_str,
                step: Some(120),
                filter: None,
                agg: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");

        // Confirm the `histogram` key is absent in JSON for scalar series.
        let raw: serde_json::Value = serde_json::from_slice(&body).expect("parse");
        let series_arr = raw["series"].as_array().expect("series array");
        assert_eq!(series_arr.len(), 1, "one scalar series");
        assert!(
            series_arr[0].get("histogram").is_none(),
            "scalar series must not have histogram field in JSON"
        );
    }
}
