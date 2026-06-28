//! Small browser utilities shared across pages.

/// Format an ISO-8601 timestamp as a human-readable relative string.
pub fn relative_time(iso: &str) -> String {
    let then_ms = match parse_iso_to_ms(iso) {
        Some(v) => v,
        None => return iso.to_string(),
    };
    let now_ms = js_sys::Date::now();
    let delta_secs = ((now_ms - then_ms) / 1000.0) as i64;

    if delta_secs < 60 {
        return "just now".to_string();
    }
    let mins = delta_secs / 60;
    if mins < 60 {
        return plural(mins, "minute");
    }
    let hours = mins / 60;
    if hours < 24 {
        return plural(hours, "hour");
    }
    let days = hours / 24;
    if days < 30 {
        return plural(days, "day");
    }
    let months = days / 30;
    if months < 12 {
        return plural(months, "month");
    }
    plural(months / 12, "year")
}

fn plural(n: i64, unit: &str) -> String {
    if n == 1 {
        format!("1 {} ago", unit)
    } else {
        format!("{} {}s ago", n, unit)
    }
}

fn parse_iso_to_ms(iso: &str) -> Option<f64> {
    let ms = js_sys::Date::parse(iso);
    if ms.is_nan() { None } else { Some(ms) }
}

/// Format bytes as a human-readable string.
pub fn fmt_bytes(bytes: i64) -> String {
    let b = bytes as f64;
    if b < 1024.0 {
        return format!("{} B", bytes);
    }
    let kb = b / 1024.0;
    if kb < 1024.0 {
        return format!("{:.1} KB", kb);
    }
    let mb = kb / 1024.0;
    if mb < 1024.0 {
        return format!("{:.1} MB", mb);
    }
    format!("{:.1} GB", mb / 1024.0)
}
