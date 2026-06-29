//! Admin and health endpoints.
//!
//! - GET /health — liveness check; exempt from bearer auth.
//! - GET /api/v1/admin/stats — system stats; behind bearer auth.

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

// ── Response types ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub db: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RetentionConfig {
    pub metrics_days: u32,
    pub logs_days: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DataCounts {
    pub series: i64,
    pub metric_points: i64,
    pub histogram_points: i64,
    pub logs: i64,
    pub spans: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatsResponse {
    pub retention: RetentionConfig,
    pub auth_required: bool,
    pub counts: DataCounts,
    pub partitions: i64,
    pub db_size_bytes: Option<i64>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /health
///
/// Liveness probe. Exempt from bearer auth (see `auth.rs`).
/// Pings the DB with `SELECT 1` and reports the result.
pub async fn health(State(state): State<Arc<AppState>>) -> Response {
    let db_ok = sqlx::query("SELECT 1")
        .execute(&state.pool)
        .await
        .is_ok();

    let resp = HealthResponse {
        status: "ok",
        db: if db_ok { "ok" } else { "error" },
        version: env!("CARGO_PKG_VERSION"),
    };

    // Return 200 even when db is "error": the server is alive; the caller can
    // inspect the `db` field. A 503 would remove it from load-balancer rotation.
    Json(resp).into_response()
}

/// GET /api/v1/admin/stats
///
/// Returns aggregate counts, retention config, partition count, and DB size.
/// Requires bearer auth when AUTH_TOKEN is set (same policy as other /api routes).
pub async fn stats(State(state): State<Arc<AppState>>) -> Response {
    let series: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM soma_observe.metric_series",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or(0);

    let metric_points: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM soma_observe.metric_point",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or(0);

    let histogram_points: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM soma_observe.metric_histogram_point",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or(0);

    let logs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM soma_observe.logs",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or(0);

    let spans: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM soma_observe.spans",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or(0);

    // Count child partitions for all partitioned tables via pg_inherits.
    let partitions: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM   pg_inherits i
        JOIN   pg_class    p  ON p.oid = i.inhparent
        JOIN   pg_namespace n ON n.oid = p.relnamespace
        WHERE  n.nspname = 'soma_observe'
          AND  p.relname IN ('metric_point', 'metric_histogram_point', 'logs', 'spans')
        "#,
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or(0);

    // pg_database_size may return NULL if the DB name lookup fails; treat as None.
    let db_size_bytes: Option<i64> = sqlx::query_scalar(
        "SELECT pg_database_size(current_database())",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or(None);

    let resp = StatsResponse {
        retention: RetentionConfig {
            metrics_days: state.config.metrics_retention_days,
            logs_days: state.config.logs_retention_days,
        },
        auth_required: state.config.auth_token.is_some(),
        counts: DataCounts {
            series,
            metric_points,
            histogram_points,
            logs,
            spans,
        },
        partitions,
        db_size_bytes,
    };

    (StatusCode::OK, Json(resp)).into_response()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use axum::body::to_bytes;

    fn test_config(auth_token: Option<String>) -> Config {
        Config {
            database_url: std::env::var("TEST_DATABASE_URL").unwrap_or_default(),
            listen_addr: "127.0.0.1:4318".into(),
            auth_token,
            metrics_retention_days: 90,
            logs_retention_days: 30,
            traces_retention_days: 7,
            cors_allow_origin: "*".into(),
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
        }
    }

    /// Returns an isolated TestDb with the schema + partitions installed,
    /// or skips the test when TEST_DATABASE_URL is not set.
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

    // ── /health ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn health_returns_ok_with_good_db() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP health_returns_ok_with_good_db: TEST_DATABASE_URL not set");
            return;
        };
        let state = Arc::new(AppState::new(db.pool.clone(), test_config(None)));
        let resp = health(State(state)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["status"], "ok");
        assert_eq!(parsed["db"], "ok");
        assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn health_version_matches_cargo() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP health_version_matches_cargo: TEST_DATABASE_URL not set");
            return;
        };
        let state = Arc::new(AppState::new(db.pool.clone(), test_config(None)));
        let resp = health(State(state)).await;
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(!parsed["version"].as_str().unwrap_or("").is_empty());
    }

    // ── /api/v1/admin/stats ───────────────────────────────────────────────────

    #[tokio::test]
    async fn stats_returns_correct_retention_and_auth_flag() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP stats_returns_correct_retention_and_auth_flag: TEST_DATABASE_URL not set");
            return;
        };
        let cfg = test_config(Some("secret".to_string()));
        let state = Arc::new(AppState::new(db.pool.clone(), cfg));
        let resp = stats(State(state)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: StatsResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.retention.metrics_days, 90);
        assert_eq!(parsed.retention.logs_days, 30);
        assert!(parsed.auth_required, "auth_required should be true when token is set");
    }

    #[tokio::test]
    async fn stats_auth_required_false_when_no_token() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP stats_auth_required_false_when_no_token: TEST_DATABASE_URL not set");
            return;
        };
        let state = Arc::new(AppState::new(db.pool.clone(), test_config(None)));
        let resp = stats(State(state)).await;
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: StatsResponse = serde_json::from_slice(&body).unwrap();
        assert!(!parsed.auth_required, "auth_required should be false when no token");
    }

    #[tokio::test]
    async fn stats_counts_are_non_negative() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP stats_counts_are_non_negative: TEST_DATABASE_URL not set");
            return;
        };
        let state = Arc::new(AppState::new(db.pool.clone(), test_config(None)));
        let resp = stats(State(state)).await;
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: StatsResponse = serde_json::from_slice(&body).unwrap();
        assert!(parsed.counts.series >= 0);
        assert!(parsed.counts.metric_points >= 0);
        assert!(parsed.counts.histogram_points >= 0);
        assert!(parsed.counts.logs >= 0);
        assert!(parsed.partitions >= 0);
    }

    #[tokio::test]
    async fn stats_db_size_bytes_present() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP stats_db_size_bytes_present: TEST_DATABASE_URL not set");
            return;
        };
        let state = Arc::new(AppState::new(db.pool.clone(), test_config(None)));
        let resp = stats(State(state)).await;
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: StatsResponse = serde_json::from_slice(&body).unwrap();
        // db_size_bytes should be populated for a real Postgres connection.
        assert!(
            parsed.db_size_bytes.is_some(),
            "db_size_bytes should be present for a live Postgres connection"
        );
        assert!(parsed.db_size_bytes.unwrap() > 0);
    }

    #[tokio::test]
    async fn stats_partitions_count_after_ensure() {
        let Some(db) = test_db().await else {
            eprintln!("SKIP stats_partitions_count_after_ensure: TEST_DATABASE_URL not set");
            return;
        };
        // ensure_partitions was called in test_db(); we expect >= 2 partitions
        // per table (current + next month) for 4 tables = at least 8.
        let state = Arc::new(AppState::new(db.pool.clone(), test_config(None)));
        let resp = stats(State(state)).await;
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: StatsResponse = serde_json::from_slice(&body).unwrap();
        assert!(
            parsed.partitions >= 8,
            "expect >= 8 child partitions after ensure (got {})",
            parsed.partitions
        );
    }

    // ── /health bypasses auth (router-level, tested via oneshot) ─────────────

    #[tokio::test]
    async fn health_route_bypasses_bearer_auth() {
        use axum::{body::Body, http::Request};
        use tower::util::ServiceExt;

        let Some(db) = test_db().await else {
            eprintln!("SKIP health_route_bypasses_bearer_auth: TEST_DATABASE_URL not set");
            return;
        };
        let cfg = Config {
            auth_token: Some("required-token".to_string()),
            ..test_config(Some("required-token".to_string()))
        };
        let state = Arc::new(AppState::new(db.pool.clone(), cfg));
        let app = crate::build_router(state);

        // No Authorization header — /health must still return 200.
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "/health must return 200 even without bearer token"
        );
    }

    #[tokio::test]
    async fn stats_route_requires_auth_when_token_set() {
        use axum::{body::Body, http::Request};
        use tower::util::ServiceExt;

        let Some(db) = test_db().await else {
            eprintln!("SKIP stats_route_requires_auth_when_token_set: TEST_DATABASE_URL not set");
            return;
        };
        let cfg = Config {
            auth_token: Some("required-token".to_string()),
            ..test_config(Some("required-token".to_string()))
        };
        let state = Arc::new(AppState::new(db.pool.clone(), cfg));
        let app = crate::build_router(state);

        // No Authorization header — /api/v1/admin/stats must return 401.
        let req = Request::builder()
            .uri("/api/v1/admin/stats")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "/api/v1/admin/stats must require bearer token"
        );
    }
}
