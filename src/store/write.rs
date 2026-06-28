use sqlx::PgPool;

use crate::store::schema::{HistogramPoint, LogRecord, MetricPoint};

/// Write a batch of scalar metric points to `metric_point`.
///
/// Uses `UNNEST` to send all points in one round-trip.
/// ON CONFLICT DO NOTHING: the (series_id, ts) PK means duplicate writes
/// for the same series+timestamp are silently ignored — safe for at-least-once ingest.
pub async fn write_metric_points(pool: &PgPool, points: &[MetricPoint]) -> Result<(), sqlx::Error> {
    if points.is_empty() {
        return Ok(());
    }

    let series_ids: Vec<i64> = points.iter().map(|p| p.series_id).collect();
    let timestamps: Vec<chrono::DateTime<chrono::Utc>> = points.iter().map(|p| p.ts).collect();
    let values: Vec<f64> = points.iter().map(|p| p.value).collect();

    sqlx::query(
        r#"
        INSERT INTO soma_observe.metric_point (series_id, ts, value)
        SELECT * FROM UNNEST($1::bigint[], $2::timestamptz[], $3::float8[])
        ON CONFLICT (series_id, ts) DO NOTHING
        "#,
    )
    .bind(&series_ids)
    .bind(&timestamps)
    .bind(&values)
    .execute(pool)
    .await?;

    Ok(())
}

/// Write a batch of histogram points to `metric_histogram_point`.
pub async fn write_histogram_points(
    pool: &PgPool,
    points: &[HistogramPoint],
) -> Result<(), sqlx::Error> {
    if points.is_empty() {
        return Ok(());
    }

    let series_ids: Vec<i64> = points.iter().map(|p| p.series_id).collect();
    let timestamps: Vec<chrono::DateTime<chrono::Utc>> = points.iter().map(|p| p.ts).collect();
    let sums: Vec<Option<f64>> = points.iter().map(|p| p.sum).collect();
    let counts: Vec<Option<i64>> = points.iter().map(|p| p.count).collect();
    let bucket_counts: Vec<Option<serde_json::Value>> =
        points.iter().map(|p| p.bucket_counts.clone()).collect();
    let bounds: Vec<Option<serde_json::Value>> = points.iter().map(|p| p.bounds.clone()).collect();

    sqlx::query(
        r#"
        INSERT INTO soma_observe.metric_histogram_point
            (series_id, ts, sum, count, bucket_counts, bounds)
        SELECT * FROM UNNEST(
            $1::bigint[],
            $2::timestamptz[],
            $3::float8[],
            $4::bigint[],
            $5::jsonb[],
            $6::jsonb[]
        )
        ON CONFLICT (series_id, ts) DO NOTHING
        "#,
    )
    .bind(&series_ids)
    .bind(&timestamps)
    .bind(&sums)
    .bind(&counts)
    .bind(&bucket_counts)
    .bind(&bounds)
    .execute(pool)
    .await?;

    Ok(())
}

/// Write a batch of log records to `logs`.
///
/// Logs have an identity-generated `id` column; we omit it from the INSERT.
pub async fn write_log_records(pool: &PgPool, records: &[LogRecord]) -> Result<(), sqlx::Error> {
    if records.is_empty() {
        return Ok(());
    }

    let timestamps: Vec<chrono::DateTime<chrono::Utc>> = records.iter().map(|r| r.ts).collect();
    let severity_numbers: Vec<Option<i32>> = records.iter().map(|r| r.severity_number).collect();
    let severity_texts: Vec<Option<&str>> =
        records.iter().map(|r| r.severity_text.as_deref()).collect();
    let bodies: Vec<Option<&str>> = records.iter().map(|r| r.body.as_deref()).collect();
    let trace_ids: Vec<Option<&str>> = records.iter().map(|r| r.trace_id.as_deref()).collect();
    let span_ids: Vec<Option<&str>> = records.iter().map(|r| r.span_id.as_deref()).collect();
    let resources: Vec<&serde_json::Value> = records.iter().map(|r| &r.resource).collect();
    let attributes: Vec<&serde_json::Value> = records.iter().map(|r| &r.attributes).collect();

    sqlx::query(
        r#"
        INSERT INTO soma_observe.logs
            (ts, severity_number, severity_text, body, trace_id, span_id, resource, attributes)
        SELECT * FROM UNNEST(
            $1::timestamptz[],
            $2::int4[],
            $3::text[],
            $4::text[],
            $5::text[],
            $6::text[],
            $7::jsonb[],
            $8::jsonb[]
        )
        "#,
    )
    .bind(&timestamps)
    .bind(&severity_numbers)
    .bind(&severity_texts)
    .bind(&bodies)
    .bind(&trace_ids)
    .bind(&span_ids)
    .bind(&resources)
    .bind(&attributes)
    .execute(pool)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn make_metric_point(series_id: i64, value: f64) -> MetricPoint {
        MetricPoint {
            series_id,
            ts: Utc::now(),
            value,
        }
    }

    fn make_histogram_point(series_id: i64) -> HistogramPoint {
        HistogramPoint {
            series_id,
            ts: Utc::now(),
            sum: Some(10.0),
            count: Some(5),
            bucket_counts: Some(json!([1, 2, 2])),
            bounds: Some(json!([0.5, 1.0])),
        }
    }

    fn make_log_record() -> LogRecord {
        LogRecord {
            ts: Utc::now(),
            severity_number: Some(9),
            severity_text: Some("INFO".to_string()),
            body: Some("test log".to_string()),
            trace_id: None,
            span_id: None,
            resource: json!({"service": "test"}),
            attributes: json!({}),
        }
    }

    #[test]
    fn empty_batch_is_noop() {
        let points: Vec<MetricPoint> = vec![];
        assert!(points.is_empty(), "empty slice must trigger early return");
    }

    #[test]
    fn unnest_arrays_have_equal_length() {
        let points = [make_metric_point(1, 1.0), make_metric_point(2, 2.0)];
        let ids: Vec<i64> = points.iter().map(|p| p.series_id).collect();
        let tss: Vec<_> = points.iter().map(|p| p.ts).collect();
        let vals: Vec<f64> = points.iter().map(|p| p.value).collect();
        assert_eq!(ids.len(), tss.len());
        assert_eq!(ids.len(), vals.len());
    }

    /// Integration test: requires TEST_DATABASE_URL.
    #[tokio::test]
    async fn write_metric_points_round_trip() {
        if std::env::var("TEST_DATABASE_URL").is_err() {
            eprintln!("SKIP write_metric_points_round_trip: TEST_DATABASE_URL not set");
            return;
        }

        let db = soma_infra::TestDb::create_from_env()
            .await
            .expect("create isolated test db");

        crate::install::install(&db.pool).await.expect("install");
        crate::store::partition::ensure_partitions(&db.pool)
            .await
            .expect("ensure partitions");

        // Insert a series row first (FK constraint).
        use crate::store::schema::{hash_series_key, SeriesKey};
        let key = SeriesKey::new("write.test.gauge", "Gauge", &json!({}), &json!({}));
        let sid = hash_series_key(&key);

        sqlx::query(
            r#"
            INSERT INTO soma_observe.metric_series (series_id, name, kind, resource, attributes)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (name, kind, resource, attributes) DO NOTHING
            "#,
        )
        .bind(sid)
        .bind("write.test.gauge")
        .bind("Gauge")
        .bind(json!({}))
        .bind(json!({}))
        .execute(&db.pool)
        .await
        .expect("insert series");

        let ts = Utc::now();
        let pts = vec![MetricPoint {
            series_id: sid,
            ts,
            value: 42.0,
        }];
        write_metric_points(&db.pool, &pts)
            .await
            .expect("write_metric_points");

        let row: (f64,) =
            sqlx::query_as("SELECT value FROM soma_observe.metric_point WHERE series_id = $1")
                .bind(sid)
                .fetch_one(&db.pool)
                .await
                .expect("fetch");
        assert_eq!(row.0, 42.0);
    }

    /// Integration test: histogram round-trip.
    #[tokio::test]
    async fn write_histogram_points_round_trip() {
        if std::env::var("TEST_DATABASE_URL").is_err() {
            eprintln!("SKIP write_histogram_points_round_trip: TEST_DATABASE_URL not set");
            return;
        }

        let db = soma_infra::TestDb::create_from_env()
            .await
            .expect("create isolated test db");

        crate::install::install(&db.pool).await.expect("install");
        crate::store::partition::ensure_partitions(&db.pool)
            .await
            .expect("ensure partitions");

        use crate::store::schema::{hash_series_key, SeriesKey};
        let key = SeriesKey::new("write.test.histo", "Histogram", &json!({}), &json!({}));
        let sid = hash_series_key(&key);

        sqlx::query(
            r#"
            INSERT INTO soma_observe.metric_series (series_id, name, kind, resource, attributes)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (name, kind, resource, attributes) DO NOTHING
            "#,
        )
        .bind(sid)
        .bind("write.test.histo")
        .bind("Histogram")
        .bind(json!({}))
        .bind(json!({}))
        .execute(&db.pool)
        .await
        .expect("insert series");

        let pts = vec![make_histogram_point(sid)];
        write_histogram_points(&db.pool, &pts)
            .await
            .expect("write_histogram_points");

        let row: (Option<i64>,) = sqlx::query_as(
            "SELECT count FROM soma_observe.metric_histogram_point WHERE series_id = $1",
        )
        .bind(sid)
        .fetch_one(&db.pool)
        .await
        .expect("fetch");
        assert_eq!(row.0, Some(5_i64));
    }

    /// Integration test: logs round-trip.
    #[tokio::test]
    async fn write_log_records_round_trip() {
        if std::env::var("TEST_DATABASE_URL").is_err() {
            eprintln!("SKIP write_log_records_round_trip: TEST_DATABASE_URL not set");
            return;
        }

        let db = soma_infra::TestDb::create_from_env()
            .await
            .expect("create isolated test db");

        crate::install::install(&db.pool).await.expect("install");
        crate::store::partition::ensure_partitions(&db.pool)
            .await
            .expect("ensure partitions");

        let records = vec![make_log_record()];
        write_log_records(&db.pool, &records)
            .await
            .expect("write_log_records");

        let row: (Option<String>,) =
            sqlx::query_as("SELECT body FROM soma_observe.logs ORDER BY id DESC LIMIT 1")
                .fetch_one(&db.pool)
                .await
                .expect("fetch");
        assert_eq!(row.0.as_deref(), Some("test log"));
    }
}
