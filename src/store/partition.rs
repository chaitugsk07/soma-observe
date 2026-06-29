//! Partition lifecycle manager for `metric_point`, `metric_histogram_point`, `logs`.
//!
//! Each table is `PARTITION BY RANGE(ts)` with monthly child partitions named
//! `<table>_y<YYYY>m<MM>`.  This module creates the current-month and
//! next-month partitions on startup and every 6 hours, and drops partitions
//! whose entire range is older than the configured retention window.
//!
//! # Future-migration note
//!
//! Any `ALTER TABLE` that changes the parent table schema (e.g. adding a
//! column, changing a column type, adding a constraint) **must also be applied
//! to every existing child partition**, because Postgres inherits the parent
//! structure but does not automatically back-propagate structural changes to
//! already-created children.  When writing such a migration, issue the `ALTER
//! TABLE` on the parent *and* run a loop over `pg_inherits` to repeat it on
//! each child, or use `ALTER TABLE … ONLY` / `ALTER TABLE … ATTACH PARTITION`
//! as appropriate.

#![forbid(unsafe_code)]

use chrono::{Datelike, Months, NaiveDate, TimeZone, Utc};
use sqlx::PgPool;
use tracing::{error, info, warn};

/// Tables whose partitions we manage.
const PARTITIONED_TABLES: &[&str] = &["metric_point", "metric_histogram_point", "logs", "spans"];

/// Create the current-month and next-month partitions for all three partitioned
/// tables if they don't already exist.
///
/// Partition naming: `<parent>_y<YYYY>m<MM>` (zero-padded month).
/// Each partition covers `[month_start, next_month_start)`.
pub async fn ensure_partitions(pool: &PgPool) -> Result<(), sqlx::Error> {
    let now = Utc::now();
    // We create the current month and next month partitions.
    let months = [
        month_start(now.year(), now.month()),
        next_month(now.year(), now.month()),
    ];

    for table in PARTITIONED_TABLES {
        for &month in &months {
            let next = advance_month(month);
            create_partition(pool, table, month, next).await?;
        }
    }

    Ok(())
}

/// Drop partitions for `table` whose entire range is older than `retention_days`.
pub async fn drop_old_partitions(
    pool: &PgPool,
    table: &str,
    retention_days: u32,
) -> Result<(), sqlx::Error> {
    let cutoff = Utc::now() - chrono::Duration::days(i64::from(retention_days));

    // Query pg_inherits + pg_class to find child partitions of this table.
    // pg_get_expr on the partition bound gives us the FROM/TO bounds.
    let rows: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT c.relname::text
        FROM   pg_inherits i
        JOIN   pg_class    c  ON c.oid = i.inhrelid
        JOIN   pg_class    p  ON p.oid = i.inhparent
        JOIN   pg_namespace n ON n.oid = p.relnamespace
        WHERE  n.nspname = 'soma_observe'
          AND  p.relname = $1
        "#,
    )
    .bind(table)
    .fetch_all(pool)
    .await?;

    for (partition_name,) in rows {
        // Partition names follow the pattern `<table>_y<YYYY>m<MM>`.
        // Parse the year/month from the suffix to determine the end of the range.
        let Some((yr, mo)) = parse_partition_ym(&partition_name) else {
            warn!(partition = %partition_name, "unexpected partition name format — skipping");
            continue;
        };

        // Partition covers [month_start, next_month_start).
        // It is fully expired when its whole range (up to next_month_start) is
        // before the retention cutoff.
        let partition_end = advance_month(month_start(yr, mo));

        if partition_end <= cutoff {
            info!(
                partition = %partition_name,
                partition_end = %partition_end,
                cutoff = %cutoff,
                "dropping expired partition"
            );
            let drop_sql = format!(r#"DROP TABLE IF EXISTS soma_observe."{partition_name}""#);
            if let Err(e) = sqlx::query(&drop_sql).execute(pool).await {
                // Log and continue — a failed drop should not prevent other
                // housekeeping work. The next run will retry.
                error!(
                    partition = %partition_name,
                    error = %e,
                    "failed to drop partition — will retry next cycle"
                );
            }
        }
    }

    Ok(())
}

/// Background task: run `ensure_partitions` + `drop_old_partitions` immediately,
/// then repeat every 6 hours.
///
/// Designed to run inside `tokio::spawn`; exits only when the process shuts down
/// (tokio runtime drops).
pub async fn run_partition_manager(
    pool: PgPool,
    metrics_retention_days: u32,
    logs_retention_days: u32,
    traces_retention_days: u32,
) {
    // Run once immediately on startup, then every 6 hours.
    let interval = std::time::Duration::from_secs(6 * 60 * 60);

    loop {
        run_once(&pool, metrics_retention_days, logs_retention_days, traces_retention_days).await;
        tokio::time::sleep(interval).await;
    }
}

/// Single cycle: ensure + drop for all tables.
async fn run_once(
    pool: &PgPool,
    metrics_retention_days: u32,
    logs_retention_days: u32,
    traces_retention_days: u32,
) {
    if let Err(e) = ensure_partitions(pool).await {
        error!(error = %e, "partition manager: ensure_partitions failed");
    } else {
        info!("partition manager: partitions ensured");
    }

    // metric_point + metric_histogram_point use metrics retention.
    for table in ["metric_point", "metric_histogram_point"] {
        if let Err(e) = drop_old_partitions(pool, table, metrics_retention_days).await {
            error!(table, error = %e, "partition manager: drop_old_partitions failed");
        }
    }

    // logs uses its own retention.
    if let Err(e) = drop_old_partitions(pool, "logs", logs_retention_days).await {
        error!(error = %e, "partition manager: drop logs partitions failed");
    }

    // spans uses traces retention.
    if let Err(e) = drop_old_partitions(pool, "spans", traces_retention_days).await {
        error!(error = %e, "partition manager: drop spans partitions failed");
    }
}

// --- helpers ---

/// Returns the UTC timestamp for the start of month (year, month).
fn month_start(year: i32, month: u32) -> chrono::DateTime<Utc> {
    Utc.from_utc_datetime(
        &NaiveDate::from_ymd_opt(year, month, 1)
            .expect("valid year/month")
            .and_hms_opt(0, 0, 0)
            .expect("midnight is valid"),
    )
}

/// Returns the timestamp for the start of the month after (year, month).
fn next_month(year: i32, month: u32) -> chrono::DateTime<Utc> {
    let start = month_start(year, month);
    advance_month(start)
}

/// Advance a month-start timestamp by exactly one month.
fn advance_month(t: chrono::DateTime<Utc>) -> chrono::DateTime<Utc> {
    t.checked_add_months(Months::new(1))
        .expect("month advance must not overflow")
}

/// Create a single partition IF NOT EXISTS.
///
/// Partition name: `<table>_y<YYYY>m<MM>` (e.g. `metric_point_y2026m06`).
/// Covers `[from, to)`.
async fn create_partition(
    pool: &PgPool,
    table: &str,
    from: chrono::DateTime<Utc>,
    to: chrono::DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let name = partition_name(table, from.year(), from.month());
    let from_str = from.format("%Y-%m-%d").to_string();
    let to_str = to.format("%Y-%m-%d").to_string();

    let sql = format!(
        r#"CREATE TABLE IF NOT EXISTS soma_observe."{name}"
           PARTITION OF soma_observe."{table}"
           FOR VALUES FROM ('{from_str}') TO ('{to_str}')"#
    );

    sqlx::query(&sql).execute(pool).await?;
    info!(partition = %name, %from_str, %to_str, "partition ensured");
    Ok(())
}

fn partition_name(table: &str, year: i32, month: u32) -> String {
    format!("{table}_y{year:04}m{month:02}")
}

/// Parse year/month from a partition name like `metric_point_y2026m06`.
/// Returns `None` if the name doesn't match the expected suffix pattern.
fn parse_partition_ym(name: &str) -> Option<(i32, u32)> {
    // Find the `_y` separator that precedes the year.
    let y_pos = name.rfind("_y")?;
    let suffix = &name[y_pos + 2..]; // e.g. "2026m06"
    let m_pos = suffix.find('m')?;
    let year: i32 = suffix[..m_pos].parse().ok()?;
    let month: u32 = suffix[m_pos + 1..].parse().ok()?;
    Some((year, month))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn partition_name_format() {
        assert_eq!(
            partition_name("metric_point", 2026, 6),
            "metric_point_y2026m06"
        );
        assert_eq!(partition_name("logs", 2025, 12), "logs_y2025m12");
    }

    #[test]
    fn parse_partition_ym_roundtrip() {
        let table = "metric_point";
        let (yr, mo) = (2026, 6);
        let name = partition_name(table, yr, mo);
        let parsed = parse_partition_ym(&name);
        assert_eq!(parsed, Some((yr, mo)));
    }

    #[test]
    fn parse_partition_ym_rejects_bad_names() {
        assert_eq!(parse_partition_ym("metric_point"), None);
        assert_eq!(parse_partition_ym("no_suffix"), None);
    }

    #[test]
    fn advance_month_wraps_december() {
        let dec = month_start(2025, 12);
        let jan = advance_month(dec);
        assert_eq!(jan.year(), 2026);
        assert_eq!(jan.month(), 1);
        assert_eq!(jan.day(), 1);
    }

    #[test]
    fn next_month_basic() {
        let nm = next_month(2026, 5);
        assert_eq!(nm.year(), 2026);
        assert_eq!(nm.month(), 6);
    }

    #[test]
    fn partition_fully_expired_when_end_before_cutoff() {
        // A June partition ended July 1 2026.
        let partition_end = month_start(2026, 7);
        // retention cutoff is Aug 1 2026 (30 days after Aug 1 is past july)
        let cutoff = month_start(2026, 8);
        assert!(partition_end <= cutoff, "june partition should be expired");
    }

    /// Integration test: needs TEST_DATABASE_URL.
    #[tokio::test]
    async fn integration_ensure_and_drop_partitions() {
        if std::env::var("TEST_DATABASE_URL").is_err() {
            eprintln!("SKIP integration_ensure_and_drop_partitions: TEST_DATABASE_URL not set");
            return;
        }

        let db = soma_infra::TestDb::create_from_env()
            .await
            .expect("create isolated test db");

        // Install the schema (idempotent).
        crate::install::install(&db.pool)
            .await
            .expect("install schema");

        // Ensure current + next month partitions are created.
        ensure_partitions(&db.pool).await.expect("ensure_partitions");

        // Verify that the current-month partition for metric_point exists by
        // inserting a series + point at the current time.
        let now = Utc::now();
        let series_id: i64 = {
            let row: (i64,) = sqlx::query_as(
                r#"
                INSERT INTO soma_observe.metric_series
                    (series_id, name, kind, resource, attributes)
                VALUES (1234567890, 'test.partition.gauge', 'Gauge', '{}', '{}')
                ON CONFLICT (name, kind, resource, attributes) DO UPDATE
                    SET name = EXCLUDED.name
                RETURNING series_id
                "#,
            )
            .fetch_one(&db.pool)
            .await
            .expect("insert series");
            row.0
        };

        // Insert a metric point — this exercises the current-month partition.
        sqlx::query(
            r#"
            INSERT INTO soma_observe.metric_point (series_id, ts, value)
            VALUES ($1, $2, 42.0)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(series_id)
        .bind(now)
        .execute(&db.pool)
        .await
        .expect("insert metric_point into partition");

        // Verify we can read it back.
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM soma_observe.metric_point WHERE series_id = $1")
                .bind(series_id)
                .fetch_one(&db.pool)
                .await
                .expect("count metric_point");
        assert!(
            count.0 >= 1,
            "metric_point insert must succeed into partition"
        );

        // Create a past partition directly and then verify drop removes it.
        // We use a date well in the past: Jan 2020.
        let old_year = 2020_i32;
        let old_month = 1_u32;
        let old_from = month_start(old_year, old_month);
        let old_to = advance_month(old_from);

        create_partition(&db.pool, "metric_point", old_from, old_to)
            .await
            .expect("create old partition");

        // Confirm it exists.
        let (exists,): (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'soma_observe' AND c.relname = $1)"
        )
        .bind(partition_name("metric_point", old_year, old_month))
        .fetch_one(&db.pool)
        .await
        .expect("check old partition exists");
        assert!(exists, "old partition must exist before drop");

        // Drop with retention_days = 1 — Jan 2020 is clearly older.
        drop_old_partitions(&db.pool, "metric_point", 1)
            .await
            .expect("drop_old_partitions");

        // Confirm old partition is gone.
        let (still_exists,): (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'soma_observe' AND c.relname = $1)"
        )
        .bind(partition_name("metric_point", old_year, old_month))
        .fetch_one(&db.pool)
        .await
        .expect("check old partition after drop");
        assert!(!still_exists, "old partition must be dropped");
    }
}
