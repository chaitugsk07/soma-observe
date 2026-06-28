use soma_infra::config::{env_or, env_parse, require_env, ConfigError};

/// Runtime configuration for soma-observe, loaded from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    /// Postgres connection URL (required). Env: `DATABASE_URL`.
    // The pool is built from this in main.rs; the field is stored so tests can read it back.
    #[allow(dead_code)]
    pub database_url: String,
    /// Address to bind the HTTP server. Env: `LISTEN_ADDR`. Default: `0.0.0.0:4318`.
    pub listen_addr: String,
    /// Optional bearer token required on ingest/query endpoints. Env: `AUTH_TOKEN`.
    pub auth_token: Option<String>,
    /// How many days to retain metric data. Env: `METRICS_RETENTION_DAYS`. Default: 90.
    pub metrics_retention_days: u32,
    /// How many days to retain log data. Env: `LOGS_RETENTION_DAYS`. Default: 30.
    pub logs_retention_days: u32,
    /// Reject datapoints with ts older than this many seconds. Env: `INGEST_WINDOW_SECS`. Default: 3600.
    pub ingest_window_secs: u64,
    /// Reject datapoints with ts more than this many seconds in the future. Env: `FUTURE_TOLERANCE_SECS`. Default: 300.
    pub future_tolerance_secs: u64,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let database_url = require_env("DATABASE_URL")?;
        let listen_addr = env_or("LISTEN_ADDR", "0.0.0.0:4318");
        let auth_token = std::env::var("AUTH_TOKEN").ok();
        let metrics_retention_days = env_parse::<u32>("METRICS_RETENTION_DAYS")?.unwrap_or(90);
        let logs_retention_days = env_parse::<u32>("LOGS_RETENTION_DAYS")?.unwrap_or(30);
        let ingest_window_secs = env_parse::<u64>("INGEST_WINDOW_SECS")?.unwrap_or(3600);
        let future_tolerance_secs = env_parse::<u64>("FUTURE_TOLERANCE_SECS")?.unwrap_or(300);
        Ok(Config {
            database_url,
            listen_addr,
            auth_token,
            metrics_retention_days,
            logs_retention_days,
            ingest_window_secs,
            future_tolerance_secs,
        })
    }
}
