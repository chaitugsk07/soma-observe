#![forbid(unsafe_code)]

mod admin;
mod alerts;
mod auth;
mod config;
mod ingest;
mod install;
mod portal;
mod query;
mod state;
mod store;

use axum::{http::Uri, middleware, routing::get, routing::post, routing::put, Router};
use config::Config;
use portal::Portal;
use state::AppState;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    soma_infra::telemetry::init();

    let cfg = Config::from_env().map_err(|e| anyhow::anyhow!("config error: {e}"))?;

    // Safety check: warn loudly if we're binding a non-loopback address without
    // an auth token — this would expose the ingest and query endpoints publicly.
    // T11: cardinality/auth note in docs.
    let is_loopback = cfg.listen_addr.starts_with("127.") || cfg.listen_addr.starts_with("[::1]");
    if !is_loopback && cfg.auth_token.is_none() {
        tracing::warn!(
            listen_addr = %cfg.listen_addr,
            "SECURITY: binding a non-loopback address with no AUTH_TOKEN set. \
             All ingest and query endpoints are unauthenticated. \
             Set AUTH_TOKEN to protect the service."
        );
    }

    // Build a pool with explicit backpressure settings:
    //   max_connections=10 (>= 2 required by install.rs advisory-lock check)
    //   acquire_timeout=5s  — fast 503 to the caller rather than waiting 30s
    // The ingest handlers already map pool-acquire errors to HTTP 503 + Retry-After.
    let mut pool_cfg = soma_infra::PoolConfig::from_env()
        .map_err(|e| anyhow::anyhow!("pool config error: {e}"))?;
    pool_cfg.max_connections = pool_cfg.max_connections.max(10);
    pool_cfg.acquire_timeout = std::time::Duration::from_secs(5);
    let pool = soma_infra::connect(&pool_cfg)
        .await
        .map_err(|e| anyhow::anyhow!("database connection failed: {e}"))?;

    install::install(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("schema install failed: {e}"))?;

    // Spawn the partition-manager background task.
    // Creates current+next monthly partitions and drops expired ones every 6h.
    let partition_pool = pool.clone();
    let metrics_ret = cfg.metrics_retention_days;
    let logs_ret = cfg.logs_retention_days;
    let traces_ret = cfg.traces_retention_days;
    tokio::spawn(async move {
        store::partition::run_partition_manager(partition_pool, metrics_ret, logs_ret, traces_ret)
            .await;
    });

    // Spawn the alert evaluator background task.
    let alert_pool = pool.clone();
    let alert_interval = cfg.alert_eval_interval_secs;
    tokio::spawn(async move {
        alerts::eval::run_alert_evaluator(alert_pool, alert_interval).await;
    });

    let app_state = Arc::new(AppState::new(pool, cfg.clone()));
    let app = build_router(app_state);

    tracing::info!(addr = %cfg.listen_addr, "soma-observe listening");
    soma_infra::web::serve_with_shutdown(&cfg.listen_addr, app)
        .await
        .map_err(|e| anyhow::anyhow!("server error: {e}"))?;

    tracing::info!("shutdown complete");
    Ok(())
}

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        // OTLP ingest endpoints (OTLP/HTTP collector standard paths)
        // Each handler handles OPTIONS itself for CORS preflight.
        .route(
            "/v1/metrics",
            post(ingest::otlp_http::ingest_metrics)
                .options(ingest::otlp_http::ingest_metrics),
        )
        .route(
            "/v1/logs",
            post(ingest::otlp_http::ingest_logs)
                .options(ingest::otlp_http::ingest_logs),
        )
        .route(
            "/v1/traces",
            post(ingest::otlp_http::ingest_traces)
                .options(ingest::otlp_http::ingest_traces),
        )
        // Query + discovery endpoints
        .route("/api/v1/metrics/query", get(query::metrics::query_metrics))
        .route(
            "/api/v1/metrics/names",
            get(query::metrics::list_metric_names),
        )
        .route(
            "/api/v1/metrics/series",
            get(query::metrics::list_metric_series),
        )
        .route("/api/v1/logs/query", get(query::logs::query_logs))
        .route("/api/v1/services", get(query::services::service_map))
        .route("/api/v1/kubernetes", get(query::kubernetes::k8s_topology))
        .route("/api/v1/traces/query", get(query::traces::query_traces))
        .route("/api/v1/traces/{trace_id}", get(query::traces::get_trace))
        // Alert CRUD + state endpoints
        .route(
            "/api/v1/alerts/rules",
            get(alerts::list_rules).post(alerts::create_rule),
        )
        .route(
            "/api/v1/alerts/rules/{id}",
            put(alerts::update_rule).delete(alerts::delete_rule),
        )
        .route("/api/v1/alerts", get(alerts::list_active_alerts))
        // Admin endpoints
        .route("/health", get(admin::health))
        .route("/api/v1/admin/stats", get(admin::stats))
        // Embedded admin portal SPA fallback (serves dashboard/dist/ at non-API paths)
        .fallback(|uri: Uri| async move { soma_infra::web::serve_spa::<Portal>(&uri) })
        // Apply optional bearer-auth middleware on all routes.
        // /health is exempt inside the middleware (see auth.rs).
        // If AUTH_TOKEN is unset the middleware is a no-op (see auth.rs).
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .with_state(state)
}
