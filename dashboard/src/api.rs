//! Async API helpers for the soma-observe admin portal.
//! All endpoints optionally use `Authorization: Bearer <token>`.
//! The token is passed in as a parameter — callers read it from AppCtx.

use serde::de::DeserializeOwned;

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (HTTP {})", self.message, self.status)
    }
}

async fn handle_response<T: DeserializeOwned>(
    resp: gloo_net::http::Response,
) -> Result<T, ApiError> {
    let status = resp.status();
    if !resp.ok() {
        let body = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or(body);
        return Err(ApiError { status, message: msg });
    }
    resp.json::<T>().await.map_err(|e| ApiError {
        status,
        message: e.to_string(),
    })
}

async fn get_json<T: DeserializeOwned>(path: &str, token: &str) -> Result<T, ApiError> {
    let mut req = gloo_net::http::Request::get(path);
    if !token.is_empty() {
        req = req.header("Authorization", &format!("Bearer {}", token));
    }
    let resp = req.send().await.map_err(|e| ApiError {
        status: 0,
        message: e.to_string(),
    })?;
    handle_response(resp).await
}

// ── Domain types ──────────────────────────────────────────────────────────────

/// GET /health
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub db: String,
    pub version: String,
}

/// GET /api/v1/admin/stats
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct RetentionConfig {
    pub metrics_days: u32,
    pub logs_days: u32,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DataCounts {
    pub series: i64,
    pub metric_points: i64,
    pub histogram_points: i64,
    pub logs: i64,
    pub spans: i64,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct StatsResponse {
    pub retention: RetentionConfig,
    pub auth_required: bool,
    pub counts: DataCounts,
    pub partitions: i64,
    pub db_size_bytes: Option<i64>,
}

/// GET /api/v1/metrics/names
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct MetricNamesResponse {
    pub names: Vec<String>,
}

/// One series entry from GET /api/v1/metrics/series
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct MetricSeries {
    pub series_id: i64,
    pub name: String,
    pub kind: String,
    pub unit: Option<String>,
    pub resource: serde_json::Value,
    pub attributes: serde_json::Value,
}

/// One query result series point from GET /api/v1/metrics/query
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct MetricPoint {
    pub start: String,
    pub end: String,
    pub value: Option<f64>,
    pub count: Option<i64>,
    /// Representative exemplar trace_id for this bucket, if any was stored.
    #[serde(default)]
    pub exemplar_trace_id: Option<String>,
}

/// Populated only for kind=Histogram series; absent for scalar series.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct HistogramSummary {
    /// Explicit bucket upper bounds (N values for N+1 buckets including +Inf).
    pub bounds: Vec<f64>,
    /// Counts from the most recent histogram point in range (len == bounds.len()+1).
    pub latest_bucket_counts: Vec<i64>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct QuerySeries {
    pub series_id: i64,
    pub resource: serde_json::Value,
    pub attributes: serde_json::Value,
    pub kind: String,
    pub points: Vec<MetricPoint>,
    /// Present only when kind == "Histogram".
    #[serde(default)]
    pub histogram: Option<HistogramSummary>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct MetricQueryResponse {
    pub metric: String,
    pub unit: Option<String>,
    pub series: Vec<QuerySeries>,
}

/// Summary of a trace from GET /api/v1/traces/query
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct TraceSummary {
    pub trace_id: String,
    pub root_name: String,
    pub root_service: Option<String>,
    pub start_time: String,
    pub duration_ms: i64,
    pub span_count: i64,
    pub status: String,
}

/// Full span detail from GET /api/v1/traces/:trace_id
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct SpanDetail {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub name: String,
    pub kind: Option<String>,
    pub service_name: Option<String>,
    pub scope_name: Option<String>,
    pub start_time: String,
    pub end_time: String,
    pub duration_ns: i64,
    pub status_code: Option<String>,
    pub status_message: Option<String>,
    pub resource: serde_json::Value,
    pub attributes: serde_json::Value,
    pub events: serde_json::Value,
    pub links: serde_json::Value,
}

/// One log record from GET /api/v1/logs/query (NDJSON)
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LogRecord {
    pub id: i64,
    pub ts: String,
    pub severity_number: Option<i32>,
    pub severity_text: Option<String>,
    pub body: Option<String>,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    pub resource: serde_json::Value,
    pub attributes: serde_json::Value,
}

// ── API functions ─────────────────────────────────────────────────────────────

/// Check server health. No auth required.
pub async fn get_health() -> bool {
    gloo_net::http::Request::get("/health")
        .send()
        .await
        .map(|r| r.ok())
        .unwrap_or(false)
}

/// GET /health — full response for overview page.
pub async fn get_health_full(token: &str) -> Result<HealthResponse, ApiError> {
    get_json::<HealthResponse>("/health", token).await
}

/// GET /api/v1/admin/stats
pub async fn get_stats(token: &str) -> Result<StatsResponse, ApiError> {
    get_json::<StatsResponse>("/api/v1/admin/stats", token).await
}

/// GET /api/v1/metrics/names
pub async fn get_metric_names(token: &str) -> Result<Vec<String>, ApiError> {
    let resp = get_json::<MetricNamesResponse>("/api/v1/metrics/names", token).await?;
    Ok(resp.names)
}

/// GET /api/v1/metrics/series?name=<name>
pub async fn get_metric_series(token: &str, name: &str) -> Result<Vec<MetricSeries>, ApiError> {
    let url = format!("/api/v1/metrics/series?name={}", urlencoded(name));
    get_json::<Vec<MetricSeries>>(&url, token).await
}

/// GET /api/v1/metrics/query
pub async fn query_metrics(
    token: &str,
    name: &str,
    start: Option<&str>,
    end: Option<&str>,
    step: Option<&str>,
    filter: Option<&str>,
    agg: Option<&str>,
) -> Result<MetricQueryResponse, ApiError> {
    let mut url = format!("/api/v1/metrics/query?name={}", urlencoded(name));
    if let Some(v) = start {
        if !v.is_empty() {
            url.push_str(&format!("&start={}", urlencoded(v)));
        }
    }
    if let Some(v) = end {
        if !v.is_empty() {
            url.push_str(&format!("&end={}", urlencoded(v)));
        }
    }
    if let Some(v) = step {
        if !v.is_empty() {
            url.push_str(&format!("&step={}", urlencoded(v)));
        }
    }
    if let Some(v) = filter {
        if !v.is_empty() {
            url.push_str(&format!("&filter={}", urlencoded(v)));
        }
    }
    if let Some(v) = agg {
        if !v.is_empty() {
            url.push_str(&format!("&agg={}", urlencoded(v)));
        }
    }
    get_json::<MetricQueryResponse>(&url, token).await
}

/// GET /api/v1/logs/query — returns NDJSON; parse each line as a LogRecord.
pub async fn query_logs(
    token: &str,
    start: Option<&str>,
    end: Option<&str>,
    filter: Option<&str>,
    severity_min: Option<&str>,
    q: Option<&str>,
    limit: Option<u32>,
    trace_id: Option<&str>,
    span_id: Option<&str>,
) -> Result<Vec<LogRecord>, ApiError> {
    let mut url = "/api/v1/logs/query?".to_string();
    let mut first = true;
    let mut push = |k: &str, v: &str| {
        if !first {
            url.push('&');
        }
        first = false;
        url.push_str(k);
        url.push('=');
        url.push_str(&urlencoded(v));
    };
    if let Some(v) = start {
        if !v.is_empty() {
            push("start", v);
        }
    }
    if let Some(v) = end {
        if !v.is_empty() {
            push("end", v);
        }
    }
    if let Some(v) = filter {
        if !v.is_empty() {
            push("filter", v);
        }
    }
    if let Some(v) = severity_min {
        if !v.is_empty() {
            push("severity_min", v);
        }
    }
    if let Some(v) = q {
        if !v.is_empty() {
            push("q", v);
        }
    }
    if let Some(n) = limit {
        push("limit", &n.to_string());
    }
    if let Some(v) = trace_id {
        if !v.is_empty() {
            push("trace_id", v);
        }
    }
    if let Some(v) = span_id {
        if !v.is_empty() {
            push("span_id", v);
        }
    }

    let mut req = gloo_net::http::Request::get(&url);
    if !token.is_empty() {
        req = req.header("Authorization", &format!("Bearer {}", token));
    }
    let resp = req.send().await.map_err(|e| ApiError {
        status: 0,
        message: e.to_string(),
    })?;
    if !resp.ok() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ApiError {
            status: resp.status(),
            message: body,
        });
    }
    let text = resp.text().await.map_err(|e| ApiError {
        status: 0,
        message: e.to_string(),
    })?;
    // Parse NDJSON: one JSON object per line.
    let records = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| match serde_json::from_str::<LogRecord>(line) {
            Ok(r) => Some(r),
            Err(e) => {
                leptos::logging::warn!("logs_query: skipping unparseable NDJSON line: {e}");
                None
            }
        })
        .collect();
    Ok(records)
}

/// Per-service RED metrics — mirrors server ServiceStats.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ServiceStats {
    pub name: String,
    pub span_count: i64,
    pub error_count: i64,
    pub error_rate: f64,
    pub rate_per_sec: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
}

/// Caller → callee dependency edge — mirrors server ServiceEdge.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ServiceEdge {
    pub from: String,
    pub to: String,
    pub call_count: i64,
    pub error_count: i64,
    pub p99_ms: f64,
}

/// Response envelope — mirrors server ServiceMapResponse.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ServiceMap {
    pub services: Vec<ServiceStats>,
    pub edges: Vec<ServiceEdge>,
}

/// GET /api/v1/services?start=&end=
pub async fn get_service_map(
    token: &str,
    start: Option<&str>,
    end: Option<&str>,
) -> Result<ServiceMap, ApiError> {
    let mut url = "/api/v1/services?".to_string();
    let mut first = true;
    let mut push = |k: &str, v: &str| {
        if !first {
            url.push('&');
        }
        first = false;
        url.push_str(&format!("{}={}", k, urlencoded(v)));
    };
    if let Some(v) = start {
        if !v.is_empty() {
            push("start", v);
        }
    }
    if let Some(v) = end {
        if !v.is_empty() {
            push("end", v);
        }
    }
    get_json::<ServiceMap>(&url, token).await
}

/// Per-workload RED metrics — mirrors server K8sWorkload.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct K8sWorkload {
    pub workload: String,
    pub kind: String,
    pub pods: Vec<String>,
    pub pod_count: i64,
    pub span_count: i64,
    pub error_count: i64,
    pub error_rate: f64,
    pub rate_per_sec: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
}

/// Namespace with workloads — mirrors server K8sNamespace.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct K8sNamespace {
    pub name: String,
    pub pod_count: i64,
    pub span_count: i64,
    pub error_count: i64,
    pub workloads: Vec<K8sWorkload>,
}

/// Response envelope — mirrors server K8sTopologyResponse.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct K8sTopology {
    pub namespaces: Vec<K8sNamespace>,
    pub node_count: i64,
}

/// GET /api/v1/kubernetes?start=&end=
pub async fn get_kubernetes_topology(
    token: &str,
    start: Option<&str>,
    end: Option<&str>,
) -> Result<K8sTopology, ApiError> {
    let mut url = "/api/v1/kubernetes?".to_string();
    let mut first = true;
    let mut push = |k: &str, v: &str| {
        if !first {
            url.push('&');
        }
        first = false;
        url.push_str(&format!("{}={}", k, urlencoded(v)));
    };
    if let Some(v) = start {
        if !v.is_empty() {
            push("start", v);
        }
    }
    if let Some(v) = end {
        if !v.is_empty() {
            push("end", v);
        }
    }
    get_json::<K8sTopology>(&url, token).await
}

/// GET /api/v1/traces/query — returns a JSON array of TraceSummary.
pub async fn query_traces(
    token: &str,
    service: Option<&str>,
    name: Option<&str>,
    status: Option<&str>,
    min_duration_ms: Option<&str>,
    max_duration_ms: Option<&str>,
    start: Option<&str>,
    end: Option<&str>,
    limit: Option<u32>,
) -> Result<Vec<TraceSummary>, ApiError> {
    let mut parts: Vec<String> = Vec::new();
    let mut kv = |k: &str, v: &str| parts.push(format!("{}={}", k, urlencoded(v)));
    if let Some(v) = service { if !v.is_empty() { kv("service", v); } }
    if let Some(v) = name { if !v.is_empty() { kv("name", v); } }
    if let Some(v) = status { if !v.is_empty() { kv("status", v); } }
    if let Some(v) = min_duration_ms { if !v.is_empty() { kv("min_duration_ms", v); } }
    if let Some(v) = max_duration_ms { if !v.is_empty() { kv("max_duration_ms", v); } }
    if let Some(v) = start { if !v.is_empty() { kv("start", v); } }
    if let Some(v) = end { if !v.is_empty() { kv("end", v); } }
    if let Some(n) = limit { kv("limit", &n.to_string()); }
    let url = format!("/api/v1/traces/query?{}", parts.join("&"));
    get_json::<Vec<TraceSummary>>(&url, token).await
}

/// GET /api/v1/traces/:trace_id — returns a JSON array of SpanDetail.
pub async fn get_trace(token: &str, trace_id: &str) -> Result<Vec<SpanDetail>, ApiError> {
    let url = format!("/api/v1/traces/{}", urlencoded(trace_id));
    get_json::<Vec<SpanDetail>>(&url, token).await
}

// ── Alert types ───────────────────────────────────────────────────────────────

/// Mirrors server `AlertStateRow` — all fields nullable except `state` and `since`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct AlertStateRow {
    pub state: String,
    pub since: String,
    pub last_value: Option<f64>,
    pub last_eval: Option<String>,
    pub last_notified: Option<String>,
    pub last_message: Option<String>,
}

/// Mirrors server `AlertRule` — `state` is nullable (None if never evaluated).
/// `config` is kept as `serde_json::Value` to avoid fighting the metric|log union;
/// the UI reads individual fields by key at display / submit time.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct AlertRule {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    pub severity: String,
    pub config: serde_json::Value,
    pub for_secs: i32,
    pub webhook_url: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    // server uses skip_serializing_if = "Option::is_none" so the field may be
    // absent in JSON — use #[serde(default)] to coerce missing → None.
    #[serde(default)]
    pub state: Option<AlertStateRow>,
}

/// Mirrors the anonymous `ActiveAlert` struct serialised by `list_active_alerts`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ActiveAlert {
    pub id: i64,
    pub name: String,
    pub severity: String,
    pub kind: String,
    pub state: String,
    pub since: String,
    pub last_value: Option<f64>,
    pub last_eval: Option<String>,
    pub last_message: Option<String>,
}

// ── Mutation helpers ──────────────────────────────────────────────────────────

async fn send_json<T: serde::de::DeserializeOwned>(
    method: &str,
    path: &str,
    token: &str,
    body: &serde_json::Value,
) -> Result<T, ApiError> {
    let builder = match method {
        "POST" => gloo_net::http::Request::post(path),
        "PUT" => gloo_net::http::Request::put(path),
        _ => unreachable!(),
    };
    let mut builder = builder.header("Content-Type", "application/json");
    if !token.is_empty() {
        builder = builder.header("Authorization", &format!("Bearer {}", token));
    }
    let resp = builder
        .body(serde_json::to_string(body).unwrap_or_default())
        .map_err(|e| ApiError { status: 0, message: e.to_string() })?
        .send()
        .await
        .map_err(|e| ApiError { status: 0, message: e.to_string() })?;
    handle_response(resp).await
}

async fn delete_req(path: &str, token: &str) -> Result<(), ApiError> {
    let mut req = gloo_net::http::Request::delete(path);
    if !token.is_empty() {
        req = req.header("Authorization", &format!("Bearer {}", token));
    }
    let resp = req.send().await.map_err(|e| ApiError { status: 0, message: e.to_string() })?;
    if resp.ok() || resp.status() == 204 {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(ApiError { status: resp.status(), message: body })
    }
}

// ── Alert API functions ───────────────────────────────────────────────────────

/// GET /api/v1/alerts/rules
pub async fn list_alert_rules(token: &str) -> Result<Vec<AlertRule>, ApiError> {
    get_json::<Vec<AlertRule>>("/api/v1/alerts/rules", token).await
}

/// POST /api/v1/alerts/rules
pub async fn create_alert_rule(
    token: &str,
    body: &serde_json::Value,
) -> Result<AlertRule, ApiError> {
    send_json::<AlertRule>("POST", "/api/v1/alerts/rules", token, body).await
}

/// PUT /api/v1/alerts/rules/{id}
pub async fn update_alert_rule(
    token: &str,
    id: i64,
    body: &serde_json::Value,
) -> Result<AlertRule, ApiError> {
    let path = format!("/api/v1/alerts/rules/{}", id);
    send_json::<AlertRule>("PUT", &path, token, body).await
}

/// DELETE /api/v1/alerts/rules/{id}
pub async fn delete_alert_rule(token: &str, id: i64) -> Result<(), ApiError> {
    let path = format!("/api/v1/alerts/rules/{}", id);
    delete_req(&path, token).await
}

/// GET /api/v1/alerts
pub async fn list_active_alerts(token: &str) -> Result<Vec<ActiveAlert>, ApiError> {
    get_json::<Vec<ActiveAlert>>("/api/v1/alerts", token).await
}

// ── Dashboard types ───────────────────────────────────────────────────────────

/// One panel definition stored inside a dashboard's panels jsonb.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Panel {
    pub title: String,
    /// Metric name to query.
    pub metric: String,
    /// Chart type: "line" | "area" | "bar" | "stat" | "sparkline"
    pub chart_type: String,
    /// Relative time range: "15m" | "1h" | "6h" | "24h"
    pub range: String,
    /// Aggregation: "avg" | "sum" | "min" | "max" | "count"
    pub agg: String,
}

/// Lightweight dashboard summary from GET /api/v1/dashboards (no panels).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DashboardSummary {
    pub id: i64,
    pub name: String,
    pub updated_at: String,
}

/// Full dashboard from GET /api/v1/dashboards/{id}.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Dashboard {
    pub id: i64,
    pub name: String,
    pub panels: Vec<Panel>,
    pub updated_at: String,
}

// ── Dashboard API functions ───────────────────────────────────────────────────

/// GET /api/v1/dashboards
pub async fn list_dashboards(token: &str) -> Result<Vec<DashboardSummary>, ApiError> {
    get_json::<Vec<DashboardSummary>>("/api/v1/dashboards", token).await
}

/// GET /api/v1/dashboards/{id}
pub async fn get_dashboard(token: &str, id: i64) -> Result<Dashboard, ApiError> {
    let path = format!("/api/v1/dashboards/{}", id);
    get_json::<Dashboard>(&path, token).await
}

/// POST /api/v1/dashboards
pub async fn create_dashboard(
    token: &str,
    name: &str,
    panels: &[Panel],
) -> Result<Dashboard, ApiError> {
    let body = serde_json::json!({ "name": name, "panels": panels });
    send_json::<Dashboard>("POST", "/api/v1/dashboards", token, &body).await
}

/// PUT /api/v1/dashboards/{id}
pub async fn update_dashboard(
    token: &str,
    id: i64,
    name: &str,
    panels: &[Panel],
) -> Result<Dashboard, ApiError> {
    let path = format!("/api/v1/dashboards/{}", id);
    let body = serde_json::json!({ "name": name, "panels": panels });
    send_json::<Dashboard>("PUT", &path, token, &body).await
}

/// DELETE /api/v1/dashboards/{id}
pub async fn delete_dashboard(token: &str, id: i64) -> Result<(), ApiError> {
    let path = format!("/api/v1/dashboards/{}", id);
    delete_req(&path, token).await
}

/// Minimal percent-encoding for query values (encodes space, &, =, +).
fn urlencoded(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            ' ' => "%20".chars().collect::<Vec<_>>(),
            '&' => "%26".chars().collect(),
            '=' => "%3D".chars().collect(),
            '+' => "%2B".chars().collect(),
            _ => vec![c],
        })
        .collect()
}
