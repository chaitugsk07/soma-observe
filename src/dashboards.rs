//! Dashboards: savable named collections of metric panels.
//!
//! panels is stored as opaque jsonb — the backend never interprets the panel
//! schema; the frontend owns it. CRUD mirrors src/alerts/mod.rs exactly.

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

use crate::state::AppState;

// ── Types ─────────────────────────────────────────────────────────────────────

/// Full row from `dashboards`, including panels.
#[derive(Debug, Serialize, Deserialize)]
pub struct Dashboard {
    pub id: i64,
    pub name: String,
    pub panels: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Lightweight summary for the list endpoint (omits panels).
#[derive(Debug, Serialize, Deserialize)]
pub struct DashboardSummary {
    pub id: i64,
    pub name: String,
    pub updated_at: DateTime<Utc>,
}

// ── Request DTOs ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateDashboardRequest {
    pub name: String,
    /// Panels stored verbatim; defaults to empty array if omitted.
    #[serde(default = "default_panels")]
    pub panels: Value,
}

#[derive(Debug, Deserialize)]
pub struct UpdateDashboardRequest {
    pub name: String,
    pub panels: Value,
}

fn default_panels() -> Value {
    Value::Array(vec![])
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /api/v1/dashboards — list all dashboards (id, name, updated_at only).
pub async fn list_dashboards(State(state): State<Arc<AppState>>) -> Response {
    type Row = (i64, String, DateTime<Utc>);

    let rows: Vec<Row> = match sqlx::query_as(
        "SELECT id, name, updated_at FROM soma_observe.dashboards ORDER BY id",
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "list_dashboards query failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let summaries: Vec<DashboardSummary> = rows
        .into_iter()
        .map(|(id, name, updated_at)| DashboardSummary { id, name, updated_at })
        .collect();
    Json(summaries).into_response()
}

/// GET /api/v1/dashboards/{id} — full dashboard including panels.
pub async fn get_dashboard(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Response {
    type Row = (i64, String, Value, DateTime<Utc>, DateTime<Utc>);

    let row: Option<Row> = match sqlx::query_as(
        "SELECT id, name, panels, created_at, updated_at FROM soma_observe.dashboards WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "get_dashboard query failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    match row {
        None => StatusCode::NOT_FOUND.into_response(),
        Some((id, name, panels, created_at, updated_at)) => {
            Json(Dashboard { id, name, panels, created_at, updated_at }).into_response()
        }
    }
}

/// POST /api/v1/dashboards — create a new dashboard.
pub async fn create_dashboard(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateDashboardRequest>,
) -> Response {
    if body.name.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "name is required").into_response();
    }

    let row: (i64,) = match sqlx::query_as(
        "INSERT INTO soma_observe.dashboards (name, panels) VALUES ($1, $2) RETURNING id",
    )
    .bind(&body.name)
    .bind(&body.panels)
    .fetch_one(&state.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "create_dashboard insert failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Return the full row.
    get_dashboard(State(state), Path(row.0)).await
}

/// PUT /api/v1/dashboards/{id} — update name + panels, bump updated_at.
pub async fn update_dashboard(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateDashboardRequest>,
) -> Response {
    if body.name.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "name is required").into_response();
    }

    let result = sqlx::query(
        "UPDATE soma_observe.dashboards SET name = $2, panels = $3, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(&body.name)
    .bind(&body.panels)
    .execute(&state.pool)
    .await;

    match result {
        Err(e) => {
            tracing::warn!(error = %e, "update_dashboard failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Ok(r) if r.rows_affected() == 0 => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => get_dashboard(State(state), Path(id)).await,
    }
}

/// DELETE /api/v1/dashboards/{id} — delete a dashboard.
pub async fn delete_dashboard(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Response {
    let result = sqlx::query("DELETE FROM soma_observe.dashboards WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await;

    match result {
        Err(e) => {
            tracing::warn!(error = %e, "delete_dashboard failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Ok(r) if r.rows_affected() == 0 => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
    }
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

    #[tokio::test]
    async fn crud_dashboard_round_trip() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP crud_dashboard_round_trip: TEST_DATABASE_URL not set");
            return;
        };
        let state = Arc::new(AppState::new(db.pool.clone(), make_cfg()));

        let panels = json!([{"title": "CPU", "metric": "cpu.usage", "chart_type": "line", "range": "1h", "agg": "avg"}]);

        // Create
        let body = CreateDashboardRequest {
            name: "test-dash".into(),
            panels: panels.clone(),
        };
        let resp = create_dashboard(State(state.clone()), Json(body)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let dash: Dashboard = serde_json::from_slice(&bytes).expect("parse dashboard");
        assert_eq!(dash.name, "test-dash");
        assert_eq!(dash.panels, panels);
        let dash_id = dash.id;

        // List — must appear
        let resp = list_dashboards(State(state.clone())).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let list: Vec<DashboardSummary> = serde_json::from_slice(&bytes).expect("parse list");
        assert!(list.iter().any(|d| d.id == dash_id));

        // Get by id
        let resp = get_dashboard(State(state.clone()), Path(dash_id)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let fetched: Dashboard = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(fetched.panels, panels);

        // Update — rename + add panel
        let new_panels = json!([
            {"title": "CPU", "metric": "cpu.usage", "chart_type": "line", "range": "1h", "agg": "avg"},
            {"title": "Mem", "metric": "mem.used", "chart_type": "area", "range": "6h", "agg": "avg"}
        ]);
        let upd = UpdateDashboardRequest {
            name: "test-dash-v2".into(),
            panels: new_panels.clone(),
        };
        let resp = update_dashboard(State(state.clone()), Path(dash_id), Json(upd)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let updated: Dashboard = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(updated.name, "test-dash-v2");
        assert_eq!(updated.panels, new_panels);

        // Delete
        let resp = delete_dashboard(State(state.clone()), Path(dash_id)).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // List — must be gone
        let resp = list_dashboards(State(state.clone())).await;
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let list: Vec<DashboardSummary> = serde_json::from_slice(&bytes).unwrap();
        assert!(!list.iter().any(|d| d.id == dash_id));
    }
}
