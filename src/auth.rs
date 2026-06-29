//! Optional bearer-token auth middleware.
//!
//! Policy: if `AUTH_TOKEN` is configured in [`crate::config::Config`], every
//! request must carry `Authorization: Bearer <token>` with the matching value.
//! If `AUTH_TOKEN` is unset the layer is a no-op and all requests pass through.
//!
//! The raw header extraction delegates to [`soma_infra::web::extract_bearer`]
//! (platform plumbing). The _decision_ (require vs. pass-through) stays here —
//! it is this service's policy, not infra's.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::state::AppState;

/// Axum middleware: enforce bearer token when `AUTH_TOKEN` is set.
///
/// Wired via `axum::middleware::from_fn_with_state` so it receives
/// the same shared `Arc<AppState>` as the route handlers.
///
/// /health is exempt: liveness probes must not require a token.
pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    // Exempt liveness probe from auth — probers have no token.
    if req.uri().path() == "/health" {
        return next.run(req).await;
    }

    let Some(ref expected) = state.config.auth_token else {
        // AUTH_TOKEN not configured — open access.
        return next.run(req).await;
    };

    let raw = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    match soma_infra::web::extract_bearer(raw) {
        Some(token) if token == expected.as_str() => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, "invalid or missing bearer token").into_response(),
    }
}

#[cfg(test)]
mod tests {
    /// Unit tests for the bearer extraction + comparison logic (no DB needed).
    /// The middleware delegates to `soma_infra::web::extract_bearer` for the raw
    /// extraction; these tests verify the extraction + comparison path.

    #[test]
    fn bearer_match() {
        let extracted = soma_infra::web::extract_bearer(Some("Bearer mysecret"));
        assert_eq!(extracted, Some("mysecret"));
        // Simulated comparison: matches expected "mysecret"
        assert_eq!(extracted, Some("mysecret"));
    }

    #[test]
    fn bearer_missing_rejects() {
        assert!(soma_infra::web::extract_bearer(None).is_none());
    }

    #[test]
    fn bearer_wrong_scheme_rejects() {
        assert!(soma_infra::web::extract_bearer(Some("Basic abc")).is_none());
    }

    #[test]
    fn bearer_wrong_token_does_not_match_expected() {
        let extracted = soma_infra::web::extract_bearer(Some("Bearer wrongtoken"));
        assert_ne!(extracted, Some("mysecret"));
    }

    /// Integration test: middleware allows open access when no token configured.
    /// Requires TEST_DATABASE_URL. Uses axum test utilities.
    #[tokio::test]
    async fn middleware_open_when_no_token() {
        use crate::{auth::auth_middleware, state::AppState};
        use axum::{body::Body, http::Request, middleware, routing::get, Router};
        use std::sync::Arc;
        use tower::util::ServiceExt;

        if std::env::var("TEST_DATABASE_URL").is_err() {
            eprintln!("SKIP middleware_open_when_no_token: TEST_DATABASE_URL not set");
            return;
        }

        let db = soma_infra::TestDb::create_from_env()
            .await
            .expect("create isolated test db");

        crate::install::install(&db.pool)
            .await
            .expect("install schema");

        let cfg = crate::config::Config {
            database_url: std::env::var("TEST_DATABASE_URL").unwrap_or_default(),
            listen_addr: "127.0.0.1:4318".into(),
            auth_token: None, // no token — open access
            metrics_retention_days: 90,
            logs_retention_days: 30,
            traces_retention_days: 7,
            cors_allow_origin: "*".into(),
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
        };
        let state = Arc::new(AppState::new(db.pool.clone(), cfg));

        let app = Router::new()
            .route("/probe", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .with_state(state);

        let req = Request::builder()
            .uri("/probe")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.expect("oneshot");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    /// Integration test: middleware rejects wrong token.
    #[tokio::test]
    async fn middleware_rejects_wrong_token() {
        use crate::{auth::auth_middleware, state::AppState};
        use axum::{body::Body, http::Request, middleware, routing::get, Router};
        use std::sync::Arc;
        use tower::util::ServiceExt;

        if std::env::var("TEST_DATABASE_URL").is_err() {
            eprintln!("SKIP middleware_rejects_wrong_token: TEST_DATABASE_URL not set");
            return;
        }

        let db = soma_infra::TestDb::create_from_env()
            .await
            .expect("create isolated test db");

        crate::install::install(&db.pool)
            .await
            .expect("install schema");

        let cfg = crate::config::Config {
            database_url: std::env::var("TEST_DATABASE_URL").unwrap_or_default(),
            listen_addr: "127.0.0.1:4318".into(),
            auth_token: Some("secret123".to_string()),
            metrics_retention_days: 90,
            logs_retention_days: 30,
            traces_retention_days: 7,
            cors_allow_origin: "*".into(),
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
        };
        let state = Arc::new(AppState::new(db.pool.clone(), cfg));

        let app = Router::new()
            .route("/probe", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .with_state(state);

        // No auth header — should 401.
        let req = Request::builder()
            .uri("/probe")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.expect("oneshot");
        assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);

        // Wrong token — should 401.
        let req = Request::builder()
            .uri("/probe")
            .header("Authorization", "Bearer wrongtoken")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.expect("oneshot");
        assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);

        // Correct token — should 200.
        let req = Request::builder()
            .uri("/probe")
            .header("Authorization", "Bearer secret123")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.expect("oneshot");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }
}
