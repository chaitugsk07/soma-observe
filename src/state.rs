use sqlx::PgPool;

use crate::config::Config;
use crate::store::series::SeriesCache;

/// Shared application state, injected into axum handlers via `State<Arc<AppState>>`.
///
/// Fields are public so handlers can destructure; Arc wraps the whole struct.
pub struct AppState {
    pub pool: PgPool,
    pub series_cache: SeriesCache,
    pub config: Config,
}

impl AppState {
    pub fn new(pool: PgPool, config: Config) -> Self {
        let series_cache = SeriesCache::new(pool.clone());
        Self {
            pool,
            series_cache,
            config,
        }
    }
}
