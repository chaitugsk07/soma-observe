//! Alert evaluator background task.
//!
//! # State machine per rule
//!
//! ```text
//! ok  ──(cond true)──> pending(since=now) ──(cond true AND since+for_secs elapsed)──> firing
//!                           │                                      │
//!                     (cond false)                         (cond false)
//!                           └──────────────────> ok <────────────-┘
//! ```
//!
//! Notifications fire only on state transitions INTO firing or INTO ok FROM firing.

use chrono::Utc;
use sqlx::PgPool;
use tracing::{error, info, warn};

use super::{notify, AlertStateRow};

/// Background loop: evaluate every `interval_secs` seconds.
/// Designed to run inside `tokio::spawn`; runs until the process shuts down.
pub async fn run_alert_evaluator(pool: PgPool, interval_secs: u64) {
    let interval = std::time::Duration::from_secs(interval_secs);
    loop {
        evaluate_once(&pool).await;
        tokio::time::sleep(interval).await;
    }
}

/// Single evaluation pass — exposed for testing.
pub async fn evaluate_once(pool: &PgPool) {
    // Load all enabled rules.
    type RuleRow = (i64, String, String, String, serde_json::Value, i32, Option<String>);
    let rules: Vec<RuleRow> = match sqlx::query_as(
        r#"
        SELECT id, name, kind, severity, config, for_secs, webhook_url
        FROM soma_observe.alert_rules
        WHERE enabled = true
        ORDER BY id
        "#,
    )
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "alert evaluator: failed to load rules");
            return;
        }
    };

    for (rule_id, name, kind, severity, config, for_secs, webhook_url) in rules {
        if let Err(e) = evaluate_rule(
            pool,
            rule_id,
            &name,
            &kind,
            &severity,
            &config,
            for_secs,
            webhook_url.as_deref(),
        )
        .await
        {
            warn!(rule_id, rule_name = %name, error = %e, "alert rule evaluation failed");
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn evaluate_rule(
    pool: &PgPool,
    rule_id: i64,
    name: &str,
    kind: &str,
    severity: &str,
    config: &serde_json::Value,
    for_secs: i32,
    webhook_url: Option<&str>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let now = Utc::now();

    // Compute current value.
    let (value, condition_met, message) = match kind {
        "metric" => eval_metric(pool, config).await?,
        "log" => eval_log(pool, config).await?,
        _ => {
            warn!(rule_id, "unknown rule kind: {kind}");
            return Ok(());
        }
    };

    // Load current state (if any).
    let current: Option<AlertStateRow> = sqlx::query_as::<
        _,
        (String, chrono::DateTime<Utc>, Option<f64>, Option<chrono::DateTime<Utc>>, Option<chrono::DateTime<Utc>>, Option<String>),
    >(
        "SELECT state, since, last_value, last_eval, last_notified, last_message FROM soma_observe.alert_state WHERE rule_id = $1",
    )
    .bind(rule_id)
    .fetch_optional(pool)
    .await?
    .map(|(st, since, lv, le, ln, lm)| AlertStateRow {
        state: st,
        since,
        last_value: lv,
        last_eval: le,
        last_notified: ln,
        last_message: lm,
    });

    let prev_state = current.as_ref().map(|s| s.state.as_str()).unwrap_or("ok");

    // Determine new state.
    let new_state = match (prev_state, condition_met) {
        ("ok", true) => {
            // for_secs=0 means fire immediately; otherwise enter pending.
            if for_secs == 0 {
                "firing"
            } else {
                "pending"
            }
        }
        ("ok", false) => "ok",
        ("pending", true) => {
            // Check if for_secs has elapsed since we entered pending.
            let pending_since = current.as_ref().map(|s| s.since).unwrap_or(now);
            let elapsed = (now - pending_since).num_seconds();
            if elapsed >= i64::from(for_secs) {
                "firing"
            } else {
                "pending"
            }
        }
        ("pending", false) => "ok",
        ("firing", true) => "firing",
        ("firing", false) => "ok",
        _ => "ok",
    };

    // Determine the `since` timestamp for the new state row.
    // Reset `since` only when the state bucket changes.
    let since = if new_state == prev_state {
        current.as_ref().map(|s| s.since).unwrap_or(now)
    } else {
        now
    };

    // Upsert alert_state.
    sqlx::query(
        r#"
        INSERT INTO soma_observe.alert_state
            (rule_id, state, since, last_value, last_eval, last_message)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (rule_id) DO UPDATE SET
            state        = EXCLUDED.state,
            since        = EXCLUDED.since,
            last_value   = EXCLUDED.last_value,
            last_eval    = EXCLUDED.last_eval,
            last_message = EXCLUDED.last_message
        "#,
    )
    .bind(rule_id)
    .bind(new_state)
    .bind(since)
    .bind(value)
    .bind(now)
    .bind(&message)
    .execute(pool)
    .await?;

    info!(
        rule_id,
        rule_name = %name,
        prev_state,
        new_state,
        value,
        condition_met,
        "alert evaluated"
    );

    // Send webhook only on state transitions that matter:
    // ok/pending -> firing, or firing -> ok.
    let should_notify = matches!(
        (prev_state, new_state),
        (_, "firing") if prev_state != "firing"
    ) || matches!((prev_state, new_state), ("firing", "ok"));

    if should_notify {
        if let Some(url) = webhook_url {
            let notif_state = if new_state == "firing" {
                "firing"
            } else {
                "resolved"
            };

            // Build the human-readable summary text (Slack/Discord/Mattermost compatible).
            let threshold = config
                .get("threshold")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let text = format!(
                "[{severity_upper}] {name} {notif_state}: value {val:.4} {cmp_word} threshold {threshold:.4}",
                severity_upper = severity.to_uppercase(),
                val = value.unwrap_or(0.0),
                cmp_word = if notif_state == "firing" { "exceeds/triggers" } else { "no longer triggers" },
            );

            let payload = serde_json::json!({
                "text": text,
                "rule": name,
                "severity": severity,
                "state": notif_state,
                "value": value,
                "threshold": threshold,
                "kind": kind,
                "timestamp": now.to_rfc3339(),
            });

            notify::send_webhook(url, &payload).await;

            // Record last_notified.
            if let Err(e) = sqlx::query(
                "UPDATE soma_observe.alert_state SET last_notified = $1 WHERE rule_id = $2",
            )
            .bind(now)
            .bind(rule_id)
            .execute(pool)
            .await
            {
                warn!(rule_id, error = %e, "failed to update last_notified");
            }
        }
    }

    Ok(())
}

// ── Metric evaluation ─────────────────────────────────────────────────────────

/// Returns (value, condition_met, message).
async fn eval_metric(
    pool: &PgPool,
    config: &serde_json::Value,
) -> Result<(Option<f64>, bool, Option<String>), Box<dyn std::error::Error + Send + Sync>> {
    let obj = config.as_object().ok_or("config not an object")?;

    let metric_name = obj
        .get("metric_name")
        .and_then(|v| v.as_str())
        .ok_or("metric_name missing")?;

    let agg = obj
        .get("agg")
        .and_then(|v| v.as_str())
        .unwrap_or("avg");

    let window_secs = obj
        .get("window_secs")
        .and_then(|v| v.as_i64())
        .unwrap_or(300);

    let comparator = obj
        .get("comparator")
        .and_then(|v| v.as_str())
        .unwrap_or("gt");

    let threshold = obj
        .get("threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let attr_filter = obj.get("filter").and_then(|v| v.as_str()).filter(|s| !s.is_empty());

    // Resolve matching series IDs.
    let series_ids: Vec<i64> = if let Some(filter_str) = attr_filter {
        // Parse filter as jsonb for @> containment.
        let filter_json: serde_json::Value = crate::query::parse_filter(Some(filter_str))
            .unwrap_or_else(|| serde_json::json!({}));
        sqlx::query_as::<_, (i64,)>(
            "SELECT series_id FROM soma_observe.metric_series WHERE name = $1 AND attributes @> $2::jsonb",
        )
        .bind(metric_name)
        .bind(&filter_json)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|(id,)| id)
        .collect()
    } else {
        sqlx::query_as::<_, (i64,)>(
            "SELECT series_id FROM soma_observe.metric_series WHERE name = $1",
        )
        .bind(metric_name)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|(id,)| id)
        .collect()
    };

    if series_ids.is_empty() {
        // No series → no data → condition not met.
        return Ok((None, false, Some("no matching series found".into())));
    }

    // Aggregate metric_point over [now-window, now).
    // Using dynamic SQL for the agg function (fixed enum, safe).
    let agg_expr = match agg {
        "sum" => "SUM(value)",
        "avg" => "AVG(value)",
        "min" => "MIN(value)",
        "max" => "MAX(value)",
        "count" => "COUNT(value)",
        _ => "AVG(value)",
    };

    let sql = format!(
        "SELECT {agg_expr} FROM soma_observe.metric_point \
         WHERE series_id = ANY($1::bigint[]) AND ts >= now() - make_interval(secs => $2::float8) AND ts < now()"
    );

    let value: Option<f64> = sqlx::query_scalar(&sql)
        .bind(&series_ids)
        .bind(window_secs as f64)
        .fetch_one(pool)
        .await?;

    let condition_met = match value {
        None => false,
        Some(v) => compare(v, threshold, comparator),
    };

    let msg = value.map(|v| {
        format!(
            "{agg}({metric_name}) = {v:.4} over {window_secs}s",
        )
    });

    Ok((value, condition_met, msg))
}

// ── Log evaluation ────────────────────────────────────────────────────────────

async fn eval_log(
    pool: &PgPool,
    config: &serde_json::Value,
) -> Result<(Option<f64>, bool, Option<String>), Box<dyn std::error::Error + Send + Sync>> {
    let obj = config.as_object().ok_or("config not an object")?;

    let window_secs = obj
        .get("window_secs")
        .and_then(|v| v.as_i64())
        .unwrap_or(300);

    let comparator = obj
        .get("comparator")
        .and_then(|v| v.as_str())
        .unwrap_or("gt");

    let threshold = obj
        .get("threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let severity_min: Option<i32> = obj.get("severity_min").and_then(|v| v.as_i64()).map(|v| v as i32);
    let q: Option<&str> = obj.get("q").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    let attr_filter = obj.get("filter").and_then(|v| v.as_str()).filter(|s| !s.is_empty());

    let filter_json: Option<serde_json::Value> = attr_filter
        .and_then(|f| crate::query::parse_filter(Some(f)));

    let count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM soma_observe.logs
        WHERE ts >= now() - make_interval(secs => $1::float8)
          AND ts < now()
          AND ($2::int4 IS NULL OR severity_number >= $2)
          AND ($3::text IS NULL OR body ILIKE '%' || $3 || '%')
          AND ($4::jsonb IS NULL OR attributes @> $4)
        "#,
    )
    .bind(window_secs as f64)
    .bind(severity_min)
    .bind(q)
    .bind(filter_json.as_ref())
    .fetch_one(pool)
    .await?;

    let value = count as f64;
    let condition_met = compare(value, threshold, comparator);
    let msg = Some(format!("log count = {count} over {window_secs}s"));

    Ok((Some(value), condition_met, msg))
}

// ── Comparison helper ─────────────────────────────────────────────────────────

fn compare(value: f64, threshold: f64, comparator: &str) -> bool {
    match comparator {
        "gt" => value > threshold,
        "lt" => value < threshold,
        "gte" => value >= threshold,
        "lte" => value <= threshold,
        _ => false,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use serde_json::json;
    use std::sync::Arc;

    fn make_cfg() -> Config {
        Config {
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

    // ── Unit tests ────────────────────────────────────────────────────────────

    #[test]
    fn compare_gt() {
        assert!(compare(5.0, 1.0, "gt"));
        assert!(!compare(1.0, 5.0, "gt"));
        assert!(!compare(1.0, 1.0, "gt"));
    }

    #[test]
    fn compare_gte() {
        assert!(compare(1.0, 1.0, "gte"));
        assert!(compare(2.0, 1.0, "gte"));
        assert!(!compare(0.0, 1.0, "gte"));
    }

    #[test]
    fn compare_lt() {
        assert!(compare(0.0, 1.0, "lt"));
        assert!(!compare(1.0, 0.0, "lt"));
    }

    #[test]
    fn compare_lte() {
        assert!(compare(1.0, 1.0, "lte"));
        assert!(!compare(2.0, 1.0, "lte"));
    }

    // ── Evaluator integration tests ───────────────────────────────────────────

    /// Seed a metric series + points above threshold, run evaluate_once, assert firing.
    #[tokio::test]
    async fn eval_metric_rule_fires_above_threshold() {
        let Some(db) = test_db().await else {
            eprintln!(
                "SKIP eval_metric_rule_fires_above_threshold: TEST_DATABASE_URL not set"
            );
            return;
        };
        let pool = &db.pool;

        // Insert a rule: avg(cpu) > 1.0 over 300s, for_secs=0 → fires immediately.
        let metric = format!(
            "alert.cpu.{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let (rule_id,): (i64,) = sqlx::query_as(
            r#"
            INSERT INTO soma_observe.alert_rules (name, kind, config, for_secs)
            VALUES ($1, 'metric', $2, 0) RETURNING id
            "#,
        )
        .bind(format!("rule-{metric}"))
        .bind(json!({
            "metric_name": metric,
            "agg": "avg",
            "comparator": "gt",
            "threshold": 1.0,
            "window_secs": 300
        }))
        .fetch_one(pool)
        .await
        .expect("insert rule");

        // Seed a metric series + point with value 50 (above threshold 1).
        // Use a deterministic series_id derived from the rule_id to avoid collisions.
        let sid = rule_id * 9999 + 1;
        sqlx::query(
            r#"
            INSERT INTO soma_observe.metric_series (series_id, name, kind, resource, attributes)
            VALUES ($1, $2, 'Gauge', '{}', '{}')
            ON CONFLICT (name, kind, resource, attributes) DO UPDATE SET name = EXCLUDED.name
            "#,
        )
        .bind(sid)
        .bind(&metric)
        .execute(pool)
        .await
        .expect("insert series");

        sqlx::query(
            "INSERT INTO soma_observe.metric_point (series_id, ts, value) VALUES ($1, now(), $2) ON CONFLICT DO NOTHING",
        )
        .bind(sid)
        .bind(50.0f64)
        .execute(pool)
        .await
        .expect("insert metric point");

        // Run one evaluation pass.
        evaluate_once(pool).await;

        // Assert state is 'firing'.
        let (state,): (String,) = sqlx::query_as(
            "SELECT state FROM soma_observe.alert_state WHERE rule_id = $1",
        )
        .bind(rule_id)
        .fetch_one(pool)
        .await
        .expect("fetch alert state");
        assert_eq!(state, "firing", "rule should be firing");

        // Assert last_value is 50.
        let (last_value,): (Option<f64>,) = sqlx::query_as(
            "SELECT last_value FROM soma_observe.alert_state WHERE rule_id = $1",
        )
        .bind(rule_id)
        .fetch_one(pool)
        .await
        .expect("fetch last_value");
        assert!(
            last_value.is_some(),
            "last_value must be populated"
        );
        assert!(
            (last_value.unwrap() - 50.0).abs() < 1.0,
            "last_value should be ~50"
        );
    }

    /// Seed metric points BELOW threshold → state stays ok.
    #[tokio::test]
    async fn eval_metric_rule_stays_ok_below_threshold() {
        let Some(db) = test_db().await else {
            eprintln!(
                "SKIP eval_metric_rule_stays_ok_below_threshold: TEST_DATABASE_URL not set"
            );
            return;
        };
        let pool = &db.pool;

        let metric = format!(
            "alert.low.{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let (rule_id,): (i64,) = sqlx::query_as(
            r#"
            INSERT INTO soma_observe.alert_rules (name, kind, config, for_secs)
            VALUES ($1, 'metric', $2, 0) RETURNING id
            "#,
        )
        .bind(format!("rule-low-{metric}"))
        .bind(json!({
            "metric_name": metric,
            "agg": "avg",
            "comparator": "gt",
            "threshold": 100.0,
            "window_secs": 300
        }))
        .fetch_one(pool)
        .await
        .expect("insert rule");

        let sid = rule_id * 9999 + 2;
        sqlx::query(
            r#"INSERT INTO soma_observe.metric_series (series_id, name, kind, resource, attributes)
            VALUES ($1, $2, 'Gauge', '{}', '{}') ON CONFLICT (name, kind, resource, attributes) DO UPDATE SET name = EXCLUDED.name"#,
        )
        .bind(sid)
        .bind(&metric)
        .execute(pool)
        .await
        .expect("insert series");

        sqlx::query(
            "INSERT INTO soma_observe.metric_point (series_id, ts, value) VALUES ($1, now(), $2) ON CONFLICT DO NOTHING",
        )
        .bind(sid)
        .bind(10.0f64) // well below threshold of 100
        .execute(pool)
        .await
        .expect("insert metric point");

        evaluate_once(pool).await;

        let (state,): (String,) = sqlx::query_as(
            "SELECT state FROM soma_observe.alert_state WHERE rule_id = $1",
        )
        .bind(rule_id)
        .fetch_one(pool)
        .await
        .expect("fetch alert state");
        assert_eq!(state, "ok", "rule should be ok when below threshold");
    }

    /// Log-count rule: seed enough logs → fires; fewer → ok.
    #[tokio::test]
    async fn eval_log_rule_fires_on_count() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP eval_log_rule_fires_on_count: TEST_DATABASE_URL not set");
            return;
        };
        let pool = &db.pool;

        let run_id = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let tag = format!("alert-log-{run_id}");

        let (rule_id,): (i64,) = sqlx::query_as(
            r#"
            INSERT INTO soma_observe.alert_rules (name, kind, config, for_secs)
            VALUES ($1, 'log', $2, 0) RETURNING id
            "#,
        )
        .bind(format!("log-rule-{run_id}"))
        .bind(json!({
            "comparator": "gt",
            "threshold": 2,
            "window_secs": 300,
            "q": tag
        }))
        .fetch_one(pool)
        .await
        .expect("insert log rule");

        // Seed 5 logs matching the tag.
        for i in 0..5_i32 {
            sqlx::query(
                "INSERT INTO soma_observe.logs (ts, severity_number, body, resource, attributes) VALUES (now(), 9, $1, '{}', '{}')",
            )
            .bind(format!("{tag} log {i}"))
            .execute(pool)
            .await
            .expect("seed log");
        }

        evaluate_once(pool).await;

        let (state,): (String,) = sqlx::query_as(
            "SELECT state FROM soma_observe.alert_state WHERE rule_id = $1",
        )
        .bind(rule_id)
        .fetch_one(pool)
        .await
        .expect("fetch state");
        assert_eq!(state, "firing", "log rule should fire when count > threshold");
    }

    /// for_secs > 0: first eval produces pending, not firing.
    #[tokio::test]
    async fn eval_for_secs_produces_pending_first() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP eval_for_secs_produces_pending_first: TEST_DATABASE_URL not set");
            return;
        };
        let pool = &db.pool;

        let metric = format!(
            "alert.forsecs.{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        // for_secs=3600 → needs 1h of sustained condition to fire.
        let (rule_id,): (i64,) = sqlx::query_as(
            r#"
            INSERT INTO soma_observe.alert_rules (name, kind, config, for_secs)
            VALUES ($1, 'metric', $2, 3600) RETURNING id
            "#,
        )
        .bind(format!("rule-forsecs-{metric}"))
        .bind(json!({
            "metric_name": metric,
                "agg": "avg",
            "comparator": "gt",
            "threshold": 1.0,
            "window_secs": 300
        }))
        .fetch_one(pool)
        .await
        .expect("insert rule");

        let sid = rule_id * 9999 + 3;
        sqlx::query(
            r#"INSERT INTO soma_observe.metric_series (series_id, name, kind, resource, attributes)
            VALUES ($1, $2, 'Gauge', '{}', '{}') ON CONFLICT (name, kind, resource, attributes) DO UPDATE SET name = EXCLUDED.name"#,
        )
        .bind(sid)
        .bind(&metric)
        .execute(pool)
        .await
        .expect("insert series");

        sqlx::query(
            "INSERT INTO soma_observe.metric_point (series_id, ts, value) VALUES ($1, now(), $2) ON CONFLICT DO NOTHING",
        )
        .bind(sid)
        .bind(999.0f64) // well above threshold
        .execute(pool)
        .await
        .expect("insert metric point");

        evaluate_once(pool).await;

        let (state,): (String,) = sqlx::query_as(
            "SELECT state FROM soma_observe.alert_state WHERE rule_id = $1",
        )
        .bind(rule_id)
        .fetch_one(pool)
        .await
        .expect("fetch state");
        assert_eq!(state, "pending", "with for_secs=3600, first eval must be pending");

        let _state_arc = Arc::new(crate::state::AppState::new(pool.clone(), make_cfg()));
    }

    /// Firing rule resolves when condition clears.
    #[tokio::test]
    async fn eval_firing_resolves_when_condition_clears() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP eval_firing_resolves_when_condition_clears: TEST_DATABASE_URL not set");
            return;
        };
        let pool = &db.pool;

        let metric = format!(
            "alert.resolve.{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let (rule_id,): (i64,) = sqlx::query_as(
            r#"
            INSERT INTO soma_observe.alert_rules (name, kind, config, for_secs)
            VALUES ($1, 'metric', $2, 0) RETURNING id
            "#,
        )
        .bind(format!("rule-resolve-{metric}"))
        .bind(json!({
            "metric_name": metric,
            "agg": "avg",
            "comparator": "gt",
            "threshold": 1.0,
            "window_secs": 300
        }))
        .fetch_one(pool)
        .await
        .expect("insert rule");

        let sid = rule_id * 9999 + 4;
        sqlx::query(
            r#"INSERT INTO soma_observe.metric_series (series_id, name, kind, resource, attributes)
            VALUES ($1, $2, 'Gauge', '{}', '{}') ON CONFLICT (name, kind, resource, attributes) DO UPDATE SET name = EXCLUDED.name"#,
        )
        .bind(sid)
        .bind(&metric)
        .execute(pool)
        .await
        .expect("insert series");

        // Insert a point above threshold and evaluate → should fire.
        sqlx::query(
            "INSERT INTO soma_observe.metric_point (series_id, ts, value) VALUES ($1, now(), $2) ON CONFLICT DO NOTHING",
        )
        .bind(sid)
        .bind(99.0f64)
        .execute(pool)
        .await
        .expect("insert high point");

        evaluate_once(pool).await;

        let (state,): (String,) = sqlx::query_as(
            "SELECT state FROM soma_observe.alert_state WHERE rule_id = $1",
        )
        .bind(rule_id)
        .fetch_one(pool)
        .await
        .expect("fetch state after firing");
        assert_eq!(state, "firing");

        // Delete the metric point and insert a low one → should resolve.
        sqlx::query("DELETE FROM soma_observe.metric_point WHERE series_id = $1")
            .bind(sid)
            .execute(pool)
            .await
            .expect("delete old points");

        sqlx::query(
            "INSERT INTO soma_observe.metric_point (series_id, ts, value) VALUES ($1, now(), $2) ON CONFLICT DO NOTHING",
        )
        .bind(sid)
        .bind(0.5f64) // below threshold
        .execute(pool)
        .await
        .expect("insert low point");

        evaluate_once(pool).await;

        let (resolved_state,): (String,) = sqlx::query_as(
            "SELECT state FROM soma_observe.alert_state WHERE rule_id = $1",
        )
        .bind(rule_id)
        .fetch_one(pool)
        .await
        .expect("fetch state after resolve");
        assert_eq!(resolved_state, "ok", "rule should resolve when condition clears");
    }
}
