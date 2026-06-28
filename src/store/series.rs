use std::sync::Mutex;

use lru::LruCache;
use sqlx::PgPool;

use crate::store::schema::{hash_series_key, Series, SeriesKey};

/// Maximum number of series entries held in the in-process LRU cache.
/// 100 000 entries ≈ a few MB — fits comfortably in a typical service instance.
const CACHE_CAPACITY: usize = 100_000;

/// Cached value per series: its resolved series_id, plus (for cumulative-monotonic
/// counters) the last observed cumulative value for delta conversion.
#[derive(Debug, Clone)]
pub struct CachedSeries {
    pub series_id: i64,
    /// Last cumulative value seen for this series.
    /// None = never seen (gauge / histogram / not-yet-seen counter).
    pub last_cumulative: Option<f64>,
}

/// In-process LRU cache mapping `SeriesKey` → `CachedSeries`.
///
/// On a cache miss, `resolve` inserts the series into the DB with
/// `INSERT … ON CONFLICT (name,kind,resource,attributes) DO NOTHING`
/// and re-SELECTs to get the canonical series_id regardless of which
/// concurrent writer won.
pub struct SeriesCache {
    pool: PgPool,
    // Mutex is held only for the in-memory lookup/insert — never across .await.
    // async-no-lock-await: we take the lock, do the lookup, release it,
    // then .await the DB call, then take the lock again to update.
    inner: Mutex<LruCache<SeriesKey, CachedSeries>>,
}

impl SeriesCache {
    pub fn new(pool: PgPool) -> Self {
        let cap = std::num::NonZeroUsize::new(CACHE_CAPACITY).expect("CACHE_CAPACITY is non-zero");
        Self {
            pool,
            inner: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Resolve the series_id for `series`, creating the series row if absent.
    ///
    /// Cache hit: returns the cached series_id without a DB round-trip.
    /// Cache miss: upserts the series row via INSERT … ON CONFLICT DO NOTHING,
    /// then re-SELECTs the series_id (handles concurrent inserts correctly).
    pub async fn resolve(&self, series: &Series) -> Result<i64, sqlx::Error> {
        let key = SeriesKey::new(
            &series.name,
            &series.kind,
            &series.resource,
            &series.attributes,
        );

        // Fast path: check cache without hitting the DB.
        {
            let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(cached) = guard.get(&key) {
                return Ok(cached.series_id);
            }
        }

        // Slow path: insert-or-no-op then re-select.
        let series_id = resolve_db(&self.pool, &key, series).await?;

        // Update cache.
        {
            let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.put(
                key,
                CachedSeries {
                    series_id,
                    last_cumulative: None,
                },
            );
        }

        Ok(series_id)
    }

    /// Update the last_cumulative value for a series_id in the cache.
    /// Used by the cumulative→delta counter conversion at ingest time.
    ///
    /// Returns the previous last_cumulative value (None if not cached or first
    /// time seen), then updates the cache entry to `new_value`.
    pub fn swap_cumulative(&self, key: &SeriesKey, new_value: f64) -> Option<f64> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = guard.get_mut(key) {
            let prev = entry.last_cumulative;
            entry.last_cumulative = Some(new_value);
            prev
        } else {
            None
        }
    }
}

/// DB-level series resolution: INSERT … ON CONFLICT DO NOTHING, then SELECT.
async fn resolve_db(pool: &PgPool, key: &SeriesKey, series: &Series) -> Result<i64, sqlx::Error> {
    // Use the hash as the series_id (content-addressed).
    // If another writer inserted first with the same content-hash, the conflict
    // is on (name,kind,resource,attributes) and DO NOTHING leaves their row.
    // The re-SELECT below returns whichever series_id won.
    let series_id = hash_series_key(key);

    sqlx::query(
        r#"
        INSERT INTO soma_observe.metric_series
            (series_id, name, kind, resource, attributes, unit)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (name, kind, resource, attributes) DO NOTHING
        "#,
    )
    .bind(series_id)
    .bind(&series.name)
    .bind(&series.kind)
    .bind(&series.resource)
    .bind(&series.attributes)
    .bind(series.unit.as_deref())
    .execute(pool)
    .await?;

    // Re-SELECT to get the canonical series_id — handles concurrent inserts.
    let row: (i64,) = sqlx::query_as(
        r#"
        SELECT series_id
        FROM soma_observe.metric_series
        WHERE name = $1 AND kind = $2 AND resource = $3 AND attributes = $4
        "#,
    )
    .bind(&series.name)
    .bind(&series.kind)
    .bind(&series.resource)
    .bind(&series.attributes)
    .fetch_one(pool)
    .await?;

    Ok(row.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_series(name: &str) -> Series {
        let resource = json!({"service": "test"});
        let attributes = json!({});
        Series {
            name: name.to_string(),
            resource,
            attributes,
            kind: "Gauge".to_string(),
            unit: None,
        }
    }

    #[test]
    fn cache_hit_returns_cached_id() {
        use std::num::NonZeroUsize;
        let cap = NonZeroUsize::new(10).unwrap();
        let inner = Mutex::new(LruCache::new(cap));
        let key = SeriesKey::new("cpu", "Gauge", &json!({}), &json!({}));
        let expected_id = 42_i64;
        {
            let mut g = inner.lock().unwrap();
            g.put(
                key.clone(),
                CachedSeries {
                    series_id: expected_id,
                    last_cumulative: None,
                },
            );
        }
        let id = {
            let mut g = inner.lock().unwrap();
            g.get(&key).map(|c| c.series_id)
        };
        assert_eq!(id, Some(expected_id));
    }

    #[test]
    fn cache_miss_returns_none_before_insert() {
        use std::num::NonZeroUsize;
        let cap = NonZeroUsize::new(10).unwrap();
        let inner: Mutex<LruCache<SeriesKey, CachedSeries>> = Mutex::new(LruCache::new(cap));
        let key = SeriesKey::new("new_metric", "Gauge", &json!({}), &json!({}));
        let id = {
            let mut g = inner.lock().unwrap();
            g.get(&key).map(|c| c.series_id)
        };
        assert_eq!(id, None, "fresh cache must miss");
    }

    #[test]
    fn swap_cumulative_tracks_last_value() {
        use std::num::NonZeroUsize;

        // Test only the in-memory LRU logic — no pool or Tokio context needed.
        let cap = NonZeroUsize::new(10).unwrap();
        let inner: Mutex<LruCache<SeriesKey, CachedSeries>> = Mutex::new(LruCache::new(cap));

        let key = SeriesKey::new("counter", "Sum", &json!({}), &json!({}));

        // Seed the cache directly.
        {
            let mut g = inner.lock().unwrap();
            g.put(
                key.clone(),
                CachedSeries {
                    series_id: 1,
                    last_cumulative: None,
                },
            );
        }

        // Helper: swap_cumulative logic inlined so we can test without a pool.
        let swap = |new_val: f64| -> Option<f64> {
            let mut g = inner.lock().unwrap();
            if let Some(entry) = g.get_mut(&key) {
                let prev = entry.last_cumulative;
                entry.last_cumulative = Some(new_val);
                prev
            } else {
                None
            }
        };

        let prev = swap(100.0);
        assert_eq!(prev, None, "first observation has no previous value");

        let prev2 = swap(150.0);
        assert_eq!(prev2, Some(100.0));
    }

    /// Integration test: needs TEST_DATABASE_URL.
    #[tokio::test]
    async fn resolve_inserts_and_caches() {
        if std::env::var("TEST_DATABASE_URL").is_err() {
            eprintln!("SKIP resolve_inserts_and_caches: TEST_DATABASE_URL not set");
            return;
        }

        let db = soma_infra::TestDb::create_from_env()
            .await
            .expect("create isolated test db");

        crate::install::install(&db.pool)
            .await
            .expect("install schema");

        let cache = SeriesCache::new(db.pool.clone());
        let series = make_series("test.metric.resolve");

        // First resolve — cache miss → DB insert.
        let id1 = cache.resolve(&series).await.expect("resolve");
        assert_ne!(id1, 0);

        // Second resolve — cache hit.
        let id2 = cache.resolve(&series).await.expect("resolve cached");
        assert_eq!(id1, id2, "cache hit must return the same series_id");
    }
}
