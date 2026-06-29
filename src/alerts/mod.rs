//! Alerting: metric-threshold and log-count rules with webhook notifications.
//!
//! # Architecture
//!
//! - `AlertRule`  — parsed configuration row (from `alert_rules` table).
//! - `eval`       — background evaluator loop; state machine ok/pending/firing.
//! - `notify`     — webhook POST on state transitions.
//! - Handlers     — CRUD for rules + state query, wired into the axum router.

pub mod eval;
mod notify;

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;

use crate::state::AppState;

// ── Types ─────────────────────────────────────────────────────────────────────

/// A row from `alert_rules` with its optional state joined in.
#[derive(Debug, Serialize, Deserialize)]
pub struct AlertRule {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    pub severity: String,
    pub config: Value,
    pub for_secs: i32,
    pub webhook_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Current evaluator state — None if never evaluated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<AlertStateRow>,
}

/// A row from `alert_state`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertStateRow {
    pub state: String,
    pub since: DateTime<Utc>,
    pub last_value: Option<f64>,
    pub last_eval: Option<DateTime<Utc>>,
    pub last_notified: Option<DateTime<Utc>>,
    pub last_message: Option<String>,
}

// ── Create / update DTOs ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateRuleRequest {
    pub name: String,
    pub kind: String,
    pub enabled: Option<bool>,
    pub severity: Option<String>,
    pub config: Value,
    pub for_secs: Option<i32>,
    pub webhook_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateRuleRequest {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub severity: Option<String>,
    pub config: Option<Value>,
    pub for_secs: Option<i32>,
    pub webhook_url: Option<Option<String>>,
}

// ── Validation ────────────────────────────────────────────────────────────────

fn valid_kind(kind: &str) -> bool {
    matches!(kind, "metric" | "log")
}

fn valid_severity(s: &str) -> bool {
    matches!(s, "info" | "warning" | "critical")
}

fn validate_metric_config(cfg: &Value) -> Result<(), &'static str> {
    let obj = cfg.as_object().ok_or("config must be an object")?;
    if obj.get("metric_name").and_then(|v| v.as_str()).is_none() {
        return Err("metric config requires 'metric_name' (string)");
    }
    let agg = obj.get("agg").and_then(|v| v.as_str()).unwrap_or("avg");
    if !matches!(agg, "avg" | "sum" | "min" | "max" | "count") {
        return Err("metric config 'agg' must be avg|sum|min|max|count");
    }
    let cmp = obj.get("comparator").and_then(|v| v.as_str()).unwrap_or("gt");
    if !matches!(cmp, "gt" | "lt" | "gte" | "lte") {
        return Err("metric config 'comparator' must be gt|lt|gte|lte");
    }
    Ok(())
}

fn validate_log_config(cfg: &Value) -> Result<(), &'static str> {
    let obj = cfg.as_object().ok_or("config must be an object")?;
    let cmp = obj.get("comparator").and_then(|v| v.as_str()).unwrap_or("gt");
    if !matches!(cmp, "gt" | "gte") {
        return Err("log config 'comparator' must be gt|gte");
    }
    Ok(())
}

fn validate_rule(kind: &str, cfg: &Value) -> Result<(), &'static str> {
    match kind {
        "metric" => validate_metric_config(cfg),
        "log" => validate_log_config(cfg),
        _ => Err("kind must be 'metric' or 'log'"),
    }
}

// ── DB helpers ────────────────────────────────────────────────────────────────

/// Load a single rule + its state (if any).
async fn load_rule(pool: &PgPool, id: i64) -> Option<AlertRule> {
    type Row = (
        i64,
        String,
        String,
        bool,
        String,
        Value,
        i32,
        Option<String>,
        DateTime<Utc>,
        DateTime<Utc>,
        // state cols (may be NULL if no state yet)
        Option<String>,
        Option<DateTime<Utc>>,
        Option<f64>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        Option<String>,
    );

    let row: Option<Row> = sqlx::query_as(
        r#"
        SELECT
            r.id, r.name, r.kind, r.enabled, r.severity, r.config, r.for_secs,
            r.webhook_url, r.created_at, r.updated_at,
            s.state, s.since, s.last_value, s.last_eval, s.last_notified, s.last_message
        FROM soma_observe.alert_rules r
        LEFT JOIN soma_observe.alert_state s ON s.rule_id = r.id
        WHERE r.id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    row.map(rule_from_row)
}

/// Convert a raw query tuple into an `AlertRule`.
#[allow(clippy::type_complexity)]
fn rule_from_row(
    (id, name, kind, enabled, severity, config, for_secs, webhook_url, created_at, updated_at,
     s_state, s_since, s_last_value, s_last_eval, s_last_notified, s_last_message): (
        i64,
        String,
        String,
        bool,
        String,
        Value,
        i32,
        Option<String>,
        DateTime<Utc>,
        DateTime<Utc>,
        Option<String>,
        Option<DateTime<Utc>>,
        Option<f64>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        Option<String>,
    ),
) -> AlertRule {
    let state = s_state.map(|st| AlertStateRow {
        state: st,
        since: s_since.unwrap_or_else(Utc::now),
        last_value: s_last_value,
        last_eval: s_last_eval,
        last_notified: s_last_notified,
        last_message: s_last_message,
    });
    AlertRule {
        id,
        name,
        kind,
        enabled,
        severity,
        config,
        for_secs,
        webhook_url,
        created_at,
        updated_at,
        state,
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /api/v1/alerts/rules — list all rules with their current state.
pub async fn list_rules(State(state): State<Arc<AppState>>) -> Response {
    type Row = (
        i64,
        String,
        String,
        bool,
        String,
        Value,
        i32,
        Option<String>,
        DateTime<Utc>,
        DateTime<Utc>,
        Option<String>,
        Option<DateTime<Utc>>,
        Option<f64>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        Option<String>,
    );

    let rows: Vec<Row> = match sqlx::query_as(
        r#"
        SELECT
            r.id, r.name, r.kind, r.enabled, r.severity, r.config, r.for_secs,
            r.webhook_url, r.created_at, r.updated_at,
            s.state, s.since, s.last_value, s.last_eval, s.last_notified, s.last_message
        FROM soma_observe.alert_rules r
        LEFT JOIN soma_observe.alert_state s ON s.rule_id = r.id
        ORDER BY r.id
        "#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "list_rules query failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let rules: Vec<AlertRule> = rows.into_iter().map(rule_from_row).collect();
    Json(rules).into_response()
}

/// POST /api/v1/alerts/rules — create a new rule.
pub async fn create_rule(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateRuleRequest>,
) -> Response {
    if !valid_kind(&body.kind) {
        return (StatusCode::BAD_REQUEST, "kind must be 'metric' or 'log'").into_response();
    }
    let severity = body.severity.as_deref().unwrap_or("warning");
    if !valid_severity(severity) {
        return (
            StatusCode::BAD_REQUEST,
            "severity must be info|warning|critical",
        )
            .into_response();
    }
    if let Err(msg) = validate_rule(&body.kind, &body.config) {
        return (StatusCode::BAD_REQUEST, msg).into_response();
    }

    let enabled = body.enabled.unwrap_or(true);
    let for_secs = body.for_secs.unwrap_or(0);

    let row: (i64,) = match sqlx::query_as(
        r#"
        INSERT INTO soma_observe.alert_rules
            (name, kind, enabled, severity, config, for_secs, webhook_url)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id
        "#,
    )
    .bind(&body.name)
    .bind(&body.kind)
    .bind(enabled)
    .bind(severity)
    .bind(&body.config)
    .bind(for_secs)
    .bind(&body.webhook_url)
    .fetch_one(&state.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "create_rule insert failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let rule = match load_rule(&state.pool, row.0).await {
        Some(r) => r,
        None => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    (StatusCode::CREATED, Json(rule)).into_response()
}

/// PUT /api/v1/alerts/rules/{id} — update an existing rule.
pub async fn update_rule(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateRuleRequest>,
) -> Response {
    // Validate severity and config if provided.
    if let Some(ref s) = body.severity {
        if !valid_severity(s) {
            return (
                StatusCode::BAD_REQUEST,
                "severity must be info|warning|critical",
            )
                .into_response();
        }
    }

    // Load existing to get current kind (needed to validate config).
    let existing = match load_rule(&state.pool, id).await {
        Some(r) => r,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    if let Some(ref cfg) = body.config {
        if let Err(msg) = validate_rule(&existing.kind, cfg) {
            return (StatusCode::BAD_REQUEST, msg).into_response();
        }
    }

    let result = sqlx::query(
        r#"
        UPDATE soma_observe.alert_rules
        SET
            name        = COALESCE($2, name),
            enabled     = COALESCE($3, enabled),
            severity    = COALESCE($4, severity),
            config      = COALESCE($5, config),
            for_secs    = COALESCE($6, for_secs),
            webhook_url = CASE WHEN $7::boolean THEN $8 ELSE webhook_url END,
            updated_at  = now()
        WHERE id = $1
        "#,
    )
    .bind(id)
    .bind(body.name.as_deref())
    .bind(body.enabled)
    .bind(body.severity.as_deref())
    .bind(body.config.as_ref())
    .bind(body.for_secs)
    // webhook_url update: pass a flag ($7) indicating whether to overwrite.
    // If body.webhook_url is Some(_) we overwrite; if None we leave as-is.
    .bind(body.webhook_url.is_some())
    .bind(body.webhook_url.as_ref().and_then(|o| o.as_deref()))
    .execute(&state.pool)
    .await;

    match result {
        Err(e) => {
            tracing::warn!(error = %e, "update_rule failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Ok(r) if r.rows_affected() == 0 => return StatusCode::NOT_FOUND.into_response(),
        Ok(_) => {}
    }

    match load_rule(&state.pool, id).await {
        Some(r) => Json(r).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// DELETE /api/v1/alerts/rules/{id} — delete a rule (cascades to alert_state).
pub async fn delete_rule(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Response {
    let result = sqlx::query("DELETE FROM soma_observe.alert_rules WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await;

    match result {
        Err(e) => {
            tracing::warn!(error = %e, "delete_rule failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Ok(r) if r.rows_affected() == 0 => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
    }
}

/// GET /api/v1/alerts — active alerts (firing or pending).
pub async fn list_active_alerts(State(state): State<Arc<AppState>>) -> Response {
    type Row = (
        i64,
        String,
        String,
        String,
        String,
        DateTime<Utc>,
        Option<f64>,
        Option<DateTime<Utc>>,
        Option<String>,
    );

    let rows: Vec<Row> = match sqlx::query_as(
        r#"
        SELECT
            r.id, r.name, r.severity, r.kind,
            s.state, s.since, s.last_value, s.last_eval, s.last_message
        FROM soma_observe.alert_state s
        JOIN soma_observe.alert_rules r ON r.id = s.rule_id
        WHERE s.state IN ('firing', 'pending')
        ORDER BY s.since DESC
        "#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "list_active_alerts query failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    #[derive(Serialize)]
    struct ActiveAlert {
        id: i64,
        name: String,
        severity: String,
        kind: String,
        state: String,
        since: DateTime<Utc>,
        last_value: Option<f64>,
        last_eval: Option<DateTime<Utc>>,
        last_message: Option<String>,
    }

    let alerts: Vec<ActiveAlert> = rows
        .into_iter()
        .map(
            |(id, name, severity, kind, st, since, last_value, last_eval, last_message)| {
                ActiveAlert {
                    id,
                    name,
                    severity,
                    kind,
                    state: st,
                    since,
                    last_value,
                    last_eval,
                    last_message,
                }
            },
        )
        .collect();

    Json(alerts).into_response()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use axum::body::to_bytes;
    use serde_json::json;

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

    // ── Validation unit tests (no DB) ─────────────────────────────────────────

    #[test]
    fn validate_metric_config_ok() {
        let cfg = json!({"metric_name": "cpu", "agg": "avg", "comparator": "gt", "threshold": 90.0, "window_secs": 300});
        assert!(validate_metric_config(&cfg).is_ok());
    }

    #[test]
    fn validate_metric_config_bad_agg() {
        let cfg = json!({"metric_name": "cpu", "agg": "median", "comparator": "gt", "threshold": 1.0, "window_secs": 60});
        assert!(validate_metric_config(&cfg).is_err());
    }

    #[test]
    fn validate_log_config_ok() {
        let cfg = json!({"comparator": "gt", "threshold": 10, "window_secs": 300});
        assert!(validate_log_config(&cfg).is_ok());
    }

    #[test]
    fn validate_log_config_bad_comparator() {
        let cfg = json!({"comparator": "lt", "threshold": 10, "window_secs": 300});
        assert!(validate_log_config(&cfg).is_err());
    }

    // ── Rule CRUD integration tests ───────────────────────────────────────────

    #[tokio::test]
    async fn crud_create_list_update_delete() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP crud_create_list_update_delete: TEST_DATABASE_URL not set");
            return;
        };
        let state = Arc::new(AppState::new(db.pool.clone(), make_cfg()));

        // Create
        let body = CreateRuleRequest {
            name: "test-metric-rule".into(),
            kind: "metric".into(),
            enabled: None,
            severity: None,
            config: json!({"metric_name": "cpu", "agg": "avg", "comparator": "gt", "threshold": 80.0, "window_secs": 300}),
            for_secs: None,
            webhook_url: None,
        };
        let resp = create_rule(State(state.clone()), Json(body)).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let rule: AlertRule = serde_json::from_slice(&bytes).expect("parse rule");
        assert_eq!(rule.name, "test-metric-rule");
        assert_eq!(rule.kind, "metric");
        assert!(rule.enabled);
        assert_eq!(rule.severity, "warning");
        let rule_id = rule.id;

        // List — our rule must appear
        let resp = list_rules(State(state.clone())).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let rules: Vec<AlertRule> = serde_json::from_slice(&bytes).expect("parse rules");
        assert!(rules.iter().any(|r| r.id == rule_id));

        // Update threshold
        let upd = UpdateRuleRequest {
            name: None,
            enabled: None,
            severity: Some("critical".into()),
            config: Some(json!({"metric_name": "cpu", "agg": "avg", "comparator": "gt", "threshold": 95.0, "window_secs": 300})),
            for_secs: None,
            webhook_url: None,
        };
        let resp = update_rule(State(state.clone()), Path(rule_id), Json(upd)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let updated: AlertRule = serde_json::from_slice(&bytes).expect("parse updated");
        assert_eq!(updated.severity, "critical");
        assert!((updated.config["threshold"].as_f64().unwrap() - 95.0).abs() < 1e-9);

        // Delete
        let resp = delete_rule(State(state.clone()), Path(rule_id)).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // List — rule must be gone
        let resp = list_rules(State(state.clone())).await;
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let rules: Vec<AlertRule> = serde_json::from_slice(&bytes).unwrap();
        assert!(!rules.iter().any(|r| r.id == rule_id));
    }

    #[tokio::test]
    async fn create_rule_bad_kind_returns_400() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP create_rule_bad_kind_returns_400: TEST_DATABASE_URL not set");
            return;
        };
        let state = Arc::new(AppState::new(db.pool.clone(), make_cfg()));
        let body = CreateRuleRequest {
            name: "bad".into(),
            kind: "trace".into(),
            enabled: None,
            severity: None,
            config: json!({}),
            for_secs: None,
            webhook_url: None,
        };
        let resp = create_rule(State(state), Json(body)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_log_rule_ok() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP create_log_rule_ok: TEST_DATABASE_URL not set");
            return;
        };
        let state = Arc::new(AppState::new(db.pool.clone(), make_cfg()));
        let body = CreateRuleRequest {
            name: "log-error-count".into(),
            kind: "log".into(),
            enabled: Some(true),
            severity: Some("warning".into()),
            config: json!({"comparator": "gt", "threshold": 10, "window_secs": 300}),
            for_secs: Some(0),
            webhook_url: None,
        };
        let resp = create_rule(State(state), Json(body)).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
    }
}
