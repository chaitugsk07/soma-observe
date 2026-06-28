use soma_schema::include_dir::include_dir;

/// Advisory lock key for soma-observe migrations.
/// MUST be unique across all soma services.
/// soma-audit uses 6020250626000001; this service uses 6020250628000002.
/// If you add another service, pick a new constant and note it here.
const ADVISORY_LOCK_KEY: i64 = 6020250628000002_i64;

static MIGRATIONS_DIR: soma_schema::include_dir::Dir =
    include_dir!("$CARGO_MANIFEST_DIR/migrations");

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("environment error: {0}")]
    Env(String),
    #[error("schema migration error: {0}")]
    Schema(#[from] soma_schema::Error),
}

/// Install the soma_observe schema and run migrations.
/// Idempotent — safe to call every time the server starts.
///
/// Pool must have max_connections >= 2 (advisory lock needs a spare connection).
pub async fn install(pool: &sqlx::PgPool) -> Result<(), InstallError> {
    if pool.options().get_max_connections() < 2 {
        return Err(InstallError::Env(
            "soma-observe requires a pool with max_connections >= 2".into(),
        ));
    }

    let driver = soma_schema::PostgresDriver::new(
        pool.clone(),
        soma_schema::PostgresConfig {
            schema: Some("soma_observe".into()),
            advisory_lock_key: ADVISORY_LOCK_KEY,
            ..Default::default()
        },
    )
    .map_err(InstallError::Schema)?;

    soma_schema::Migrator::from_embedded(&MIGRATIONS_DIR)
        .map_err(InstallError::Schema)?
        .up(&driver)
        .await
        .map_err(InstallError::Schema)
}
