//! Rust types mirroring the soma_observe DB schema.
//! These are the domain types passed between ingest, store, and query layers.

/// A metric series — uniquely identifies a name + resource + attribute set + kind.
/// The `series_id` is resolved via `SeriesCache::resolve`, not stored here.
#[derive(Debug, Clone)]
pub struct Series {
    pub name: String,
    pub resource: serde_json::Value,
    pub attributes: serde_json::Value,
    pub kind: String,
    pub unit: Option<String>,
}

/// A single scalar metric data point (gauge or delta sum).
#[derive(Debug, Clone)]
pub struct MetricPoint {
    pub series_id: i64,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub value: f64,
    /// Optional exemplar: a representative trace_id sampled at measurement time.
    pub exemplar_trace_id: Option<String>,
    /// Optional exemplar: span_id paired with exemplar_trace_id.
    pub exemplar_span_id: Option<String>,
}

/// A histogram data point.
#[derive(Debug, Clone)]
pub struct HistogramPoint {
    pub series_id: i64,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub sum: Option<f64>,
    pub count: Option<i64>,
    /// Bucket counts as a JSON array of integers.
    pub bucket_counts: Option<serde_json::Value>,
    /// Explicit bounds as a JSON array of floats.
    pub bounds: Option<serde_json::Value>,
}

/// A structured log record.
#[derive(Debug, Clone)]
pub struct LogRecord {
    pub ts: chrono::DateTime<chrono::Utc>,
    pub severity_number: Option<i32>,
    pub severity_text: Option<String>,
    pub body: Option<String>,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    pub resource: serde_json::Value,
    pub attributes: serde_json::Value,
}

/// A distributed trace span.
#[derive(Debug, Clone)]
pub struct SpanRecord {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub name: String,
    pub kind: Option<String>,
    pub service_name: Option<String>,
    pub scope_name: Option<String>,
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub end_time: chrono::DateTime<chrono::Utc>,
    pub duration_ns: i64,
    pub status_code: Option<String>,
    pub status_message: Option<String>,
    pub resource: serde_json::Value,
    pub attributes: serde_json::Value,
    pub events: serde_json::Value,
    pub links: serde_json::Value,
}

/// The key used to identify a metric series uniquely.
/// Canonicalized from (name, kind, resource attrs, datapoint attrs).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SeriesKey {
    pub name: String,
    pub kind: String,
    /// Canonical sorted-key JSON string of resource attributes.
    pub resource_canonical: String,
    /// Canonical sorted-key JSON string of datapoint attributes.
    pub attributes_canonical: String,
}

impl SeriesKey {
    /// Build a SeriesKey from raw OTLP components, canonicalizing JSON.
    pub fn new(
        name: impl Into<String>,
        kind: impl Into<String>,
        resource: &serde_json::Value,
        attributes: &serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            kind: kind.into(),
            resource_canonical: canonical_json(resource),
            attributes_canonical: canonical_json(attributes),
        }
    }
}

/// Produce a canonical JSON string with object keys sorted.
/// This ensures the same logical JSON object always produces the same bytes,
/// regardless of insertion order.
pub fn canonical_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(map) => {
            let mut sorted: Vec<_> = map.iter().collect();
            sorted.sort_by_key(|(k, _)| k.as_str());
            let entries: Vec<String> = sorted
                .into_iter()
                .map(|(k, val)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_default(),
                        canonical_json(val)
                    )
                })
                .collect();
            format!("{{{}}}", entries.join(","))
        }
        serde_json::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(canonical_json).collect();
            format!("[{}]", items.join(","))
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Compute a stable i64 series_id from a SeriesKey using FNV-1a 64-bit.
///
/// FNV-1a is deterministic, platform-independent, and produces consistent
/// output across process restarts — suitable for a content-addressed series_id.
/// ponytail: 10-line inline implementation avoids adding a hash crate.
pub fn hash_series_key(key: &SeriesKey) -> i64 {
    const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
    const FNV_PRIME: u64 = 1_099_511_628_211;

    let mut h: u64 = FNV_OFFSET;
    for component in [
        key.name.as_str(),
        "\x00",
        key.kind.as_str(),
        "\x00",
        key.resource_canonical.as_str(),
        "\x00",
        key.attributes_canonical.as_str(),
    ] {
        for byte in component.as_bytes() {
            h ^= u64::from(*byte);
            h = h.wrapping_mul(FNV_PRIME);
        }
    }
    // Cast to i64 — wrapping is fine; we only need uniqueness/stability.
    h as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hash_is_stable_and_deterministic() {
        let key = SeriesKey::new(
            "cpu.usage",
            "Gauge",
            &json!({"service": "api", "host": "a"}),
            &json!({"cpu": "0"}),
        );
        let id1 = hash_series_key(&key);
        let id2 = hash_series_key(&key);
        assert_eq!(id1, id2, "hash must be deterministic");
    }

    #[test]
    fn hash_differs_for_different_keys() {
        let k1 = SeriesKey::new("cpu.usage", "Gauge", &json!({}), &json!({}));
        let k2 = SeriesKey::new("mem.usage", "Gauge", &json!({}), &json!({}));
        assert_ne!(hash_series_key(&k1), hash_series_key(&k2));
    }

    #[test]
    fn canonical_json_sorts_keys() {
        // Different insertion order → same canonical string
        let a = json!({"z": 1, "a": 2});
        let b = json!({"a": 2, "z": 1});
        assert_eq!(canonical_json(&a), canonical_json(&b));
    }

    #[test]
    fn series_key_resource_order_irrelevant() {
        let k1 = SeriesKey::new("m", "Sum", &json!({"b": 2, "a": 1}), &json!({}));
        let k2 = SeriesKey::new("m", "Sum", &json!({"a": 1, "b": 2}), &json!({}));
        assert_eq!(hash_series_key(&k1), hash_series_key(&k2));
    }
}
