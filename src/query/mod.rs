pub mod kubernetes;
pub mod logs;
pub mod metrics;
pub mod services;
pub mod traces;

use chrono::{DateTime, Utc};
use serde_json::Value;

/// Parse a time string as RFC3339 or Unix seconds.
pub(crate) fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(secs) = s.parse::<i64>() {
        return DateTime::from_timestamp(secs, 0);
    }
    None
}

/// Parse an attribute filter string of the form `key="value",key2="value2"`.
/// Returns a serde_json object suitable for `@>` jsonb containment check.
/// Returns `None` if the filter is absent or empty.
pub(crate) fn parse_filter(filter: Option<&str>) -> Option<Value> {
    let s = filter?.trim();
    if s.is_empty() {
        return None;
    }
    let mut map = serde_json::Map::new();
    for part in s.split(',') {
        let part = part.trim();
        let eq = part.find('=')?;
        let key = part[..eq].trim();
        let val_raw = part[eq + 1..].trim();
        let val = if val_raw.starts_with('"') && val_raw.ends_with('"') && val_raw.len() >= 2 {
            &val_raw[1..val_raw.len() - 1]
        } else {
            val_raw
        };
        map.insert(key.to_string(), serde_json::json!(val));
    }
    if map.is_empty() {
        None
    } else {
        Some(Value::Object(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_time_rfc3339() {
        assert!(parse_time("2024-01-01T00:00:00Z").is_some());
    }

    #[test]
    fn parse_time_unix_secs() {
        assert!(parse_time("1704067200").is_some());
    }

    #[test]
    fn parse_time_invalid() {
        assert!(parse_time("not-a-date").is_none());
    }

    #[test]
    fn parse_filter_key_value() {
        let f = parse_filter(Some(r#"env="prod",svc="api""#)).unwrap();
        assert_eq!(f["env"], "prod");
        assert_eq!(f["svc"], "api");
    }

    #[test]
    fn parse_filter_multi() {
        let f = parse_filter(Some(r#"env="prod",region="us""#)).unwrap();
        assert_eq!(f["env"], "prod");
        assert_eq!(f["region"], "us");
    }

    #[test]
    fn parse_filter_none_on_empty() {
        assert!(parse_filter(None).is_none());
        assert!(parse_filter(Some("")).is_none());
    }
}
