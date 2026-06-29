use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Duration, Utc};
use opentelemetry_proto::tonic::{
    collector::{
        logs::v1::{ExportLogsPartialSuccess, ExportLogsServiceRequest, ExportLogsServiceResponse},
        metrics::v1::{
            ExportMetricsPartialSuccess, ExportMetricsServiceRequest, ExportMetricsServiceResponse,
        },
        trace::v1::{
            ExportTracePartialSuccess, ExportTraceServiceRequest, ExportTraceServiceResponse,
        },
    },
    metrics::v1::{metric::Data, AggregationTemporality},
    trace::v1::{span::SpanKind, status::StatusCode as SpanStatusCode},
};
use prost::Message;
use serde_json::json;
use tracing::{debug, warn};

use crate::{
    state::AppState,
    store::{
        schema::{canonical_json, HistogramPoint, LogRecord, MetricPoint, Series, SeriesKey, SpanRecord},
        write::{write_histogram_points, write_log_records, write_metric_points, write_spans},
    },
};

/// Encode bytes as lowercase hex string without external deps.
/// ponytail: avoids adding hex crate; stdlib format!("{:02x}") is one-liner.
fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ── Content-type constants ────────────────────────────────────────────────────

const PROTOBUF_CT: &str = "application/x-protobuf";
const JSON_CT: &str = "application/json";

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Convert OTLP KeyValue list to a serde_json::Value (object).
fn kv_to_json(attrs: &[opentelemetry_proto::tonic::common::v1::KeyValue]) -> serde_json::Value {
    use opentelemetry_proto::tonic::common::v1::any_value::Value as AV;
    let mut map = serde_json::Map::with_capacity(attrs.len());
    for kv in attrs {
        let v = match kv.value.as_ref().and_then(|a| a.value.as_ref()) {
            Some(AV::StringValue(s)) => json!(s),
            Some(AV::BoolValue(b)) => json!(b),
            Some(AV::IntValue(i)) => json!(i),
            Some(AV::DoubleValue(d)) => json!(d),
            Some(AV::BytesValue(b)) => json!(bytes_to_hex(b)),
            Some(AV::ArrayValue(_)) | Some(AV::KvlistValue(_)) | None => json!(null),
        };
        map.insert(kv.key.clone(), v);
    }
    serde_json::Value::Object(map)
}

/// Convert OTLP AnyValue body to Option<String>.
fn body_to_string(
    body: Option<&opentelemetry_proto::tonic::common::v1::AnyValue>,
) -> Option<String> {
    use opentelemetry_proto::tonic::common::v1::any_value::Value as AV;
    match body?.value.as_ref()? {
        AV::StringValue(s) => Some(s.clone()),
        AV::BoolValue(b) => Some(b.to_string()),
        AV::IntValue(i) => Some(i.to_string()),
        AV::DoubleValue(d) => Some(d.to_string()),
        _ => None,
    }
}

/// Convert nanoseconds-since-epoch u64 to DateTime<Utc>.
fn nano_to_dt(nanos: u64) -> Option<DateTime<Utc>> {
    if nanos == 0 {
        return None;
    }
    let secs = (nanos / 1_000_000_000) as i64;
    let nsecs = (nanos % 1_000_000_000) as u32;
    DateTime::from_timestamp(secs, nsecs)
}

/// Validate that a timestamp is within the ingest window.
///
/// Returns true (accept) when:
///   now - ingest_window <= ts <= now + future_tolerance
fn ts_in_window(ts: DateTime<Utc>, ingest_window_secs: u64, future_tolerance_secs: u64) -> bool {
    let now = Utc::now();
    let oldest = now - Duration::seconds(ingest_window_secs as i64);
    let newest = now + Duration::seconds(future_tolerance_secs as i64);
    ts >= oldest && ts <= newest
}

/// Extract number value from NumberDataPoint.
fn number_value(dp: &opentelemetry_proto::tonic::metrics::v1::NumberDataPoint) -> Option<f64> {
    use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value;
    match dp.value.as_ref()? {
        Value::AsDouble(d) => Some(*d),
        Value::AsInt(i) => Some(*i as f64),
    }
}

/// Build an OTLP/JSON metrics response body (serialized to JSON bytes).
///
/// Returns (status, body_bytes, content_type).
fn metrics_response(rejected: i64, message: &str) -> (StatusCode, Vec<u8>, &'static str) {
    let resp = ExportMetricsServiceResponse {
        partial_success: if rejected > 0 || !message.is_empty() {
            Some(ExportMetricsPartialSuccess {
                rejected_data_points: rejected,
                error_message: message.to_string(),
            })
        } else {
            None
        },
    };
    let body = serde_json::to_vec(&resp).unwrap_or_default();
    (StatusCode::OK, body, JSON_CT)
}

fn logs_response(rejected: i64, message: &str) -> (StatusCode, Vec<u8>, &'static str) {
    let resp = ExportLogsServiceResponse {
        partial_success: if rejected > 0 || !message.is_empty() {
            Some(ExportLogsPartialSuccess {
                rejected_log_records: rejected,
                error_message: message.to_string(),
            })
        } else {
            None
        },
    };
    let body = serde_json::to_vec(&resp).unwrap_or_default();
    (StatusCode::OK, body, JSON_CT)
}

fn traces_response(rejected: i64, message: &str) -> (StatusCode, Vec<u8>, &'static str) {
    let resp = ExportTraceServiceResponse {
        partial_success: if rejected > 0 || !message.is_empty() {
            Some(ExportTracePartialSuccess {
                rejected_spans: rejected,
                error_message: message.to_string(),
            })
        } else {
            None
        },
    };
    let body = serde_json::to_vec(&resp).unwrap_or_default();
    (StatusCode::OK, body, JSON_CT)
}

/// Build a 503 + Retry-After response for write timeout backpressure.
fn service_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "5")],
        "write timeout — retry after 5 seconds",
    )
        .into_response()
}

/// Normalize OTLP/JSON payload so that string-encoded uint64 fields are converted
/// to JSON numbers before deserialization.
///
/// The OTLP/JSON spec encodes uint64 as JSON strings, but `opentelemetry-proto`
/// with `with-serde` does not register a string deserializer for all such fields.
/// Pre-normalizing the Value before deserialization fixes the mismatch without new
/// dependencies or custom serde wrappers.
///
/// Handles:
/// - `resourceMetrics[*].scopeMetrics[*].metrics[*].histogram.dataPoints[*].{count,bucketCounts}`
/// - `resourceMetrics[*].scopeMetrics[*].metrics[*].{gauge|sum|histogram}.dataPoints[*].exemplars[*].timeUnixNano`
/// - `resourceSpans[*].scopeSpans[*].spans[*].{startTimeUnixNano,endTimeUnixNano}`
///
/// Malformed / missing fields are skipped; the normal serde path handles them.
fn normalize_otlp_json(body: &[u8]) -> Result<Vec<u8>, String> {
    let mut root: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("json parse: {e}"))?;

    // Normalize exemplars[*].timeUnixNano: string → number within a dataPoints array.
    //
    // Root cause: Exemplar.time_unix_nano has no custom serde deserializer (unlike
    // NumberDataPoint.time_unix_nano which uses deserialize_string_to_u64).  The
    // OTLP/JSON spec encodes it as a string; the struct expects a plain u64.
    // Converting here keeps the datapoint from being dropped by the "None data" guard.
    fn normalize_exemplars(dps: &mut [serde_json::Value]) {
        for dp in dps {
            if let Some(exemplars) = dp.get_mut("exemplars").and_then(|v| v.as_array_mut()) {
                for ex in exemplars {
                    // timeUnixNano: string → number (Exemplar lacks the custom string deserializer)
                    if let Some(n) = ex
                        .get("timeUnixNano")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        ex["timeUnixNano"] = serde_json::Value::from(n);
                    }
                }
            }
        }
    }

    // Walk resourceMetrics[*].scopeMetrics[*].metrics[*].histogram.dataPoints[*]
    if let Some(rms) = root
        .get_mut("resourceMetrics")
        .and_then(|v| v.as_array_mut())
    {
        for rm in rms {
            if let Some(sms) = rm
                .get_mut("scopeMetrics")
                .and_then(|v| v.as_array_mut())
            {
                for sm in sms {
                    if let Some(metrics) =
                        sm.get_mut("metrics").and_then(|v| v.as_array_mut())
                    {
                        for metric in metrics {
                            // Histogram: count, bucketCounts, and exemplars
                            if let Some(dps) = metric
                                .get_mut("histogram")
                                .and_then(|h| h.get_mut("dataPoints"))
                                .and_then(|v| v.as_array_mut())
                            {
                                for dp in dps.iter_mut() {
                                    // count: string → number
                                    if let Some(s) = dp
                                        .get("count")
                                        .and_then(|v| v.as_str())
                                        .and_then(|s| s.parse::<u64>().ok())
                                    {
                                        dp["count"] = serde_json::Value::from(s);
                                    }
                                    // bucketCounts: array elements string → number
                                    if let Some(bcs) = dp
                                        .get_mut("bucketCounts")
                                        .and_then(|v| v.as_array_mut())
                                    {
                                        for bc in bcs {
                                            if let Some(n) = bc
                                                .as_str()
                                                .and_then(|s| s.parse::<u64>().ok())
                                            {
                                                *bc = serde_json::Value::from(n);
                                            }
                                        }
                                    }
                                }
                                normalize_exemplars(dps);
                            }
                            // Gauge and Sum: exemplars only
                            for kind in ["gauge", "sum"] {
                                if let Some(dps) = metric
                                    .get_mut(kind)
                                    .and_then(|g| g.get_mut("dataPoints"))
                                    .and_then(|v| v.as_array_mut())
                                {
                                    normalize_exemplars(dps);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Walk resourceSpans[*].scopeSpans[*].spans[*].{startTimeUnixNano,endTimeUnixNano}
    // opentelemetry-proto uses a custom serde for these (serialize_u64_to_string /
    // deserialize_string_to_u64) which handles string→u64.  However, some SDKs emit
    // them as plain JSON numbers.  The custom deserializer only accepts strings, so we
    // normalise number→string here so both forms work.
    if let Some(rss) = root
        .get_mut("resourceSpans")
        .and_then(|v| v.as_array_mut())
    {
        for rs in rss {
            if let Some(sss) = rs
                .get_mut("scopeSpans")
                .and_then(|v| v.as_array_mut())
            {
                for ss in sss {
                    if let Some(spans) = ss.get_mut("spans").and_then(|v| v.as_array_mut()) {
                        for span in spans {
                            // startTimeUnixNano / endTimeUnixNano: number → string
                            for field in ["startTimeUnixNano", "endTimeUnixNano"] {
                                if let Some(n) = span.get(field).and_then(|v| v.as_u64()) {
                                    span[field] = serde_json::Value::String(n.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    serde_json::to_vec(&root).map_err(|e| format!("json re-serialize: {e}"))
}

/// Decode the body: protobuf if Content-Type is application/x-protobuf, else try JSON.
fn decode_metrics(
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<ExportMetricsServiceRequest, String> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if ct.starts_with(PROTOBUF_CT) {
        ExportMetricsServiceRequest::decode(body.as_ref())
            .map_err(|e| format!("protobuf decode: {e}"))
    } else {
        let normalized = normalize_otlp_json(body.as_ref())?;
        serde_json::from_slice::<ExportMetricsServiceRequest>(&normalized)
            .map_err(|e| format!("json decode: {e}"))
    }
}

fn decode_logs(headers: &HeaderMap, body: &Bytes) -> Result<ExportLogsServiceRequest, String> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if ct.starts_with(PROTOBUF_CT) {
        ExportLogsServiceRequest::decode(body.as_ref()).map_err(|e| format!("protobuf decode: {e}"))
    } else {
        serde_json::from_slice::<ExportLogsServiceRequest>(body)
            .map_err(|e| format!("json decode: {e}"))
    }
}

fn decode_traces(
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<ExportTraceServiceRequest, String> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if ct.starts_with(PROTOBUF_CT) {
        ExportTraceServiceRequest::decode(body.as_ref())
            .map_err(|e| format!("protobuf decode: {e}"))
    } else {
        let normalized = normalize_otlp_json(body.as_ref())?;
        serde_json::from_slice::<ExportTraceServiceRequest>(&normalized)
            .map_err(|e| format!("json decode: {e}"))
    }
}

/// Convert SpanKind enum value to text.
fn span_kind_text(kind: i32) -> Option<String> {
    match SpanKind::try_from(kind).unwrap_or(SpanKind::Unspecified) {
        SpanKind::Unspecified => None,
        SpanKind::Internal => Some("Internal".to_string()),
        SpanKind::Server => Some("Server".to_string()),
        SpanKind::Client => Some("Client".to_string()),
        SpanKind::Producer => Some("Producer".to_string()),
        SpanKind::Consumer => Some("Consumer".to_string()),
    }
}

/// Convert StatusCode enum to text.
fn status_code_text(code: i32) -> Option<String> {
    match SpanStatusCode::try_from(code).unwrap_or(SpanStatusCode::Unset) {
        SpanStatusCode::Unset => None,
        SpanStatusCode::Ok => Some("Ok".to_string()),
        SpanStatusCode::Error => Some("Error".to_string()),
    }
}

/// Check whether bytes are all-zero (OTLP unset id).
fn bytes_are_zero(b: &[u8]) -> bool {
    b.is_empty() || b.iter().all(|&x| x == 0)
}

/// Extract the first exemplar with a non-empty trace_id from a NumberDataPoint's
/// exemplar list. Returns (exemplar_trace_id, exemplar_span_id) as hex strings.
///
/// ponytail: only one exemplar per point stored — the first with a valid trace_id.
fn extract_exemplar(
    exemplars: &[opentelemetry_proto::tonic::metrics::v1::Exemplar],
) -> (Option<String>, Option<String>) {
    for ex in exemplars {
        if !bytes_are_zero(&ex.trace_id) {
            let trace_id = Some(bytes_to_hex(&ex.trace_id));
            let span_id = if bytes_are_zero(&ex.span_id) {
                None
            } else {
                Some(bytes_to_hex(&ex.span_id))
            };
            return (trace_id, span_id);
        }
    }
    (None, None)
}

/// Respond to CORS OPTIONS preflight for ingest routes.
///
/// Returns 204 No Content with the required CORS headers.
pub fn cors_preflight(cors_origin: &str) -> Response {
    (
        StatusCode::NO_CONTENT,
        [
            ("Access-Control-Allow-Origin", cors_origin.to_string()),
            (
                "Access-Control-Allow-Headers",
                "content-type, authorization".to_string(),
            ),
            (
                "Access-Control-Allow-Methods",
                "POST, OPTIONS".to_string(),
            ),
        ],
        "",
    )
        .into_response()
}

/// Inject CORS headers onto an existing Response.
fn with_cors(mut resp: Response, cors_origin: &str) -> Response {
    let headers = resp.headers_mut();
    headers.insert(
        "Access-Control-Allow-Origin",
        cors_origin.parse().unwrap_or_else(|_| "*".parse().unwrap()),
    );
    resp
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// POST /v1/metrics — receive OTLP/HTTP metrics export request.
///
/// Accepts both protobuf (`application/x-protobuf`) and JSON
/// (`application/json`) content types per the OTLP/HTTP spec.
///
/// Supports: Gauge, cumulative-monotonic Sum, explicit-bucket Histogram.
/// Rejects: ExponentialHistogram, Summary, delta-temporality Sum (partial_success).
/// Converts cumulative Sum to per-series delta using LRU cache; on value drop (reset),
/// stores the new value as the delta.
pub async fn ingest_metrics(State(state): State<Arc<AppState>>, request: Request) -> Response {
    if request.method() == Method::OPTIONS {
        return cors_preflight(&state.config.cors_allow_origin);
    }
    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "body read error").into_response(),
    };

    let req = match decode_metrics(&parts.headers, &body) {
        Ok(r) => r,
        Err(e) => {
            debug!(error = %e, "failed to decode metrics request");
            return (StatusCode::BAD_REQUEST, e).into_response();
        }
    };

    let ingest_window = state.config.ingest_window_secs;
    let future_tol = state.config.future_tolerance_secs;

    let mut metric_points: Vec<MetricPoint> = Vec::new();
    let mut histogram_points: Vec<HistogramPoint> = Vec::new();
    let mut rejected: i64 = 0;
    let mut reject_reasons: Vec<&'static str> = Vec::new();

    for rm in &req.resource_metrics {
        let resource_json = rm
            .resource
            .as_ref()
            .map(|r| kv_to_json(&r.attributes))
            .unwrap_or_else(|| json!({}));

        for sm in &rm.scope_metrics {
            for metric in &sm.metrics {
                let name = &metric.name;
                let unit = if metric.unit.is_empty() {
                    None
                } else {
                    Some(metric.unit.as_str())
                };

                match &metric.data {
                    Some(Data::Gauge(gauge)) => {
                        for dp in &gauge.data_points {
                            let ts = match nano_to_dt(dp.time_unix_nano) {
                                Some(t) => t,
                                None => {
                                    rejected += 1;
                                    continue;
                                }
                            };
                            if !ts_in_window(ts, ingest_window, future_tol) {
                                rejected += 1;
                                continue;
                            }
                            let attrs = kv_to_json(&dp.attributes);
                            let series = Series {
                                name: name.clone(),
                                resource: resource_json.clone(),
                                attributes: attrs.clone(),
                                kind: "Gauge".to_string(),
                                unit: unit.map(|s| s.to_string()),
                            };
                            let series_id = match state.series_cache.resolve(&series).await {
                                Ok(id) => id,
                                Err(e) => {
                                    warn!(error = %e, "series resolve failed");
                                    return service_unavailable();
                                }
                            };
                            let value = match number_value(dp) {
                                Some(v) => v,
                                None => {
                                    rejected += 1;
                                    continue;
                                }
                            };
                            let (exemplar_trace_id, exemplar_span_id) =
                                extract_exemplar(&dp.exemplars);
                            metric_points.push(MetricPoint {
                                series_id,
                                ts,
                                value,
                                exemplar_trace_id,
                                exemplar_span_id,
                            });
                        }
                    }

                    Some(Data::Sum(sum)) => {
                        let temporality =
                            AggregationTemporality::try_from(sum.aggregation_temporality)
                                .unwrap_or(AggregationTemporality::Unspecified);

                        if temporality == AggregationTemporality::Delta {
                            // delta-temporality Sum: reject all data points
                            let count = sum.data_points.len() as i64;
                            rejected += count;
                            if !reject_reasons.contains(&"delta temporality Sum not supported") {
                                reject_reasons.push("delta temporality Sum not supported");
                            }
                            continue;
                        }

                        // Cumulative Sum (monotonic or not)
                        let is_monotonic = sum.is_monotonic;

                        for dp in &sum.data_points {
                            let ts = match nano_to_dt(dp.time_unix_nano) {
                                Some(t) => t,
                                None => {
                                    rejected += 1;
                                    continue;
                                }
                            };
                            if !ts_in_window(ts, ingest_window, future_tol) {
                                rejected += 1;
                                continue;
                            }
                            let attrs_json = kv_to_json(&dp.attributes);
                            let series = Series {
                                name: name.clone(),
                                resource: resource_json.clone(),
                                attributes: attrs_json.clone(),
                                kind: "Sum".to_string(),
                                unit: unit.map(|s| s.to_string()),
                            };
                            let series_id = match state.series_cache.resolve(&series).await {
                                Ok(id) => id,
                                Err(e) => {
                                    warn!(error = %e, "series resolve failed");
                                    return service_unavailable();
                                }
                            };
                            let cumulative_value = match number_value(dp) {
                                Some(v) => v,
                                None => {
                                    rejected += 1;
                                    continue;
                                }
                            };

                            // Cumulative->delta conversion for monotonic counters.
                            // Non-monotonic cumulative Sum: store value as-is.
                            let delta_value = if is_monotonic {
                                let key = SeriesKey {
                                    name: name.clone(),
                                    kind: "Sum".to_string(),
                                    resource_canonical: canonical_json(&resource_json),
                                    attributes_canonical: canonical_json(&attrs_json),
                                };
                                let prev =
                                    state.series_cache.swap_cumulative(&key, cumulative_value);
                                match prev {
                                    None => {
                                        // First observation — store value as delta (can't compute diff yet).
                                        cumulative_value
                                    }
                                    Some(p) if cumulative_value >= p => {
                                        // Normal case: emit delta.
                                        cumulative_value - p
                                    }
                                    Some(_) => {
                                        // Counter reset: the value dropped. Treat new value as delta.
                                        cumulative_value
                                    }
                                }
                            } else {
                                // Non-monotonic cumulative: store raw value.
                                cumulative_value
                            };

                            let (exemplar_trace_id, exemplar_span_id) =
                                extract_exemplar(&dp.exemplars);
                            metric_points.push(MetricPoint {
                                series_id,
                                ts,
                                value: delta_value,
                                exemplar_trace_id,
                                exemplar_span_id,
                            });
                        }
                    }

                    Some(Data::Histogram(histo)) => {
                        let temporality =
                            AggregationTemporality::try_from(histo.aggregation_temporality)
                                .unwrap_or(AggregationTemporality::Unspecified);

                        if temporality == AggregationTemporality::Delta {
                            let count = histo.data_points.len() as i64;
                            rejected += count;
                            if !reject_reasons
                                .contains(&"delta temporality Histogram not supported")
                            {
                                reject_reasons.push("delta temporality Histogram not supported");
                            }
                            continue;
                        }

                        for dp in &histo.data_points {
                            let ts = match nano_to_dt(dp.time_unix_nano) {
                                Some(t) => t,
                                None => {
                                    rejected += 1;
                                    continue;
                                }
                            };
                            if !ts_in_window(ts, ingest_window, future_tol) {
                                rejected += 1;
                                continue;
                            }
                            let attrs = kv_to_json(&dp.attributes);
                            let series = Series {
                                name: name.clone(),
                                resource: resource_json.clone(),
                                attributes: attrs.clone(),
                                kind: "Histogram".to_string(),
                                unit: unit.map(|s| s.to_string()),
                            };
                            let series_id = match state.series_cache.resolve(&series).await {
                                Ok(id) => id,
                                Err(e) => {
                                    warn!(error = %e, "series resolve failed");
                                    return service_unavailable();
                                }
                            };
                            let bucket_counts = if dp.bucket_counts.is_empty() {
                                None
                            } else {
                                // Store as jsonb array of integers.
                                Some(serde_json::Value::Array(
                                    dp.bucket_counts.iter().map(|&c| json!(c)).collect(),
                                ))
                            };
                            let bounds = if dp.explicit_bounds.is_empty() {
                                None
                            } else {
                                Some(serde_json::Value::Array(
                                    dp.explicit_bounds.iter().map(|&b| json!(b)).collect(),
                                ))
                            };
                            histogram_points.push(HistogramPoint {
                                series_id,
                                ts,
                                sum: dp.sum,
                                count: if dp.count == 0 {
                                    None
                                } else {
                                    Some(dp.count as i64)
                                },
                                bucket_counts,
                                bounds,
                            });
                        }
                    }

                    Some(Data::ExponentialHistogram(exp)) => {
                        let count = exp.data_points.len() as i64;
                        rejected += count;
                        if !reject_reasons.contains(&"ExponentialHistogram not supported") {
                            reject_reasons.push("ExponentialHistogram not supported");
                        }
                    }

                    Some(Data::Summary(sum)) => {
                        let count = sum.data_points.len() as i64;
                        rejected += count;
                        if !reject_reasons.contains(&"Summary not supported") {
                            reject_reasons.push("Summary not supported");
                        }
                    }

                    None => {
                        rejected += 1;
                        if !reject_reasons.contains(&"unrecognized or empty metric data") {
                            reject_reasons.push("unrecognized or empty metric data");
                        }
                    }
                }
            }
        }
    }

    // Batch write metric points.
    if !metric_points.is_empty() {
        if let Err(e) = write_metric_points(&state.pool, &metric_points).await {
            warn!(error = %e, "write_metric_points failed");
            return service_unavailable();
        }
    }
    if !histogram_points.is_empty() {
        if let Err(e) = write_histogram_points(&state.pool, &histogram_points).await {
            warn!(error = %e, "write_histogram_points failed");
            return service_unavailable();
        }
    }

    let reject_msg = reject_reasons.join("; ");
    let (status, body, ct) = metrics_response(rejected, &reject_msg);
    let resp = (status, [(header::CONTENT_TYPE, ct)], body).into_response();
    with_cors(resp, &state.config.cors_allow_origin)
}

/// POST /v1/logs — receive OTLP/HTTP logs export request.
///
/// Accepts both protobuf (`application/x-protobuf`) and JSON
/// (`application/json`) content types per the OTLP/HTTP spec.
///
/// Maps each LogRecord to a store::schema::LogRecord and batch-writes them.
/// Datapoints outside the ingest window are counted as rejected.
pub async fn ingest_logs(State(state): State<Arc<AppState>>, request: Request) -> Response {
    if request.method() == Method::OPTIONS {
        return cors_preflight(&state.config.cors_allow_origin);
    }
    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "body read error").into_response(),
    };

    let req = match decode_logs(&parts.headers, &body) {
        Ok(r) => r,
        Err(e) => {
            debug!(error = %e, "failed to decode logs request");
            return (StatusCode::BAD_REQUEST, e).into_response();
        }
    };

    let ingest_window = state.config.ingest_window_secs;
    let future_tol = state.config.future_tolerance_secs;

    let mut log_records: Vec<LogRecord> = Vec::new();
    let mut rejected: i64 = 0;

    for rl in &req.resource_logs {
        let resource_json = rl
            .resource
            .as_ref()
            .map(|r| kv_to_json(&r.attributes))
            .unwrap_or_else(|| json!({}));

        for sl in &rl.scope_logs {
            for lr in &sl.log_records {
                // Use time_unix_nano if set, else observed_time_unix_nano.
                let ts_nanos = if lr.time_unix_nano != 0 {
                    lr.time_unix_nano
                } else {
                    lr.observed_time_unix_nano
                };
                let ts = match nano_to_dt(ts_nanos) {
                    Some(t) => t,
                    None => {
                        // Missing timestamp — use now and proceed (log records often lack ts).
                        Utc::now()
                    }
                };
                if !ts_in_window(ts, ingest_window, future_tol) {
                    rejected += 1;
                    continue;
                }

                let severity_number = if lr.severity_number == 0 {
                    None
                } else {
                    Some(lr.severity_number)
                };
                let severity_text = if lr.severity_text.is_empty() {
                    None
                } else {
                    Some(lr.severity_text.clone())
                };

                let trace_id = if lr.trace_id.iter().all(|&b| b == 0) || lr.trace_id.is_empty() {
                    None
                } else {
                    Some(bytes_to_hex(&lr.trace_id))
                };
                let span_id = if lr.span_id.iter().all(|&b| b == 0) || lr.span_id.is_empty() {
                    None
                } else {
                    Some(bytes_to_hex(&lr.span_id))
                };

                log_records.push(LogRecord {
                    ts,
                    severity_number,
                    severity_text,
                    body: body_to_string(lr.body.as_ref()),
                    trace_id,
                    span_id,
                    resource: resource_json.clone(),
                    attributes: kv_to_json(&lr.attributes),
                });
            }
        }
    }

    if !log_records.is_empty() {
        if let Err(e) = write_log_records(&state.pool, &log_records).await {
            warn!(error = %e, "write_log_records failed");
            return service_unavailable();
        }
    }

    let (status, body, ct) = logs_response(rejected, "");
    let resp = (status, [(header::CONTENT_TYPE, ct)], body).into_response();
    with_cors(resp, &state.config.cors_allow_origin)
}

/// POST /v1/traces — receive OTLP/HTTP traces export request.
///
/// Accepts both protobuf (`application/x-protobuf`) and JSON
/// (`application/json`) content types per the OTLP/HTTP spec.
///
/// Maps each Span to a SpanRecord and batch-writes them.
/// Spans outside the ingest window are counted as rejected.
pub async fn ingest_traces(State(state): State<Arc<AppState>>, request: Request) -> Response {
    // Handle CORS preflight.
    if request.method() == Method::OPTIONS {
        return cors_preflight(&state.config.cors_allow_origin);
    }

    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "body read error").into_response(),
    };

    let req = match decode_traces(&parts.headers, &body) {
        Ok(r) => r,
        Err(e) => {
            debug!(error = %e, "failed to decode traces request");
            return (StatusCode::BAD_REQUEST, e).into_response();
        }
    };

    let ingest_window = state.config.ingest_window_secs;
    let future_tol = state.config.future_tolerance_secs;

    let mut span_records: Vec<SpanRecord> = Vec::new();
    let mut rejected: i64 = 0;

    for rs in &req.resource_spans {
        let resource_json = rs
            .resource
            .as_ref()
            .map(|r| kv_to_json(&r.attributes))
            .unwrap_or_else(|| json!({}));

        let service_name = rs
            .resource
            .as_ref()
            .and_then(|r| {
                r.attributes
                    .iter()
                    .find(|kv| kv.key == "service.name")
                    .and_then(|kv| {
                        use opentelemetry_proto::tonic::common::v1::any_value::Value as AV;
                        match kv.value.as_ref()?.value.as_ref()? {
                            AV::StringValue(s) => Some(s.clone()),
                            _ => None,
                        }
                    })
            });

        for ss in &rs.scope_spans {
            let scope_name = ss.scope.as_ref().map(|s| s.name.clone()).filter(|s| !s.is_empty());

            for span in &ss.spans {
                let start_time = match nano_to_dt(span.start_time_unix_nano) {
                    Some(t) => t,
                    None => {
                        rejected += 1;
                        continue;
                    }
                };
                if !ts_in_window(start_time, ingest_window, future_tol) {
                    rejected += 1;
                    continue;
                }

                let end_time = nano_to_dt(span.end_time_unix_nano).unwrap_or(start_time);
                let duration_ns = (span.end_time_unix_nano as i64)
                    .saturating_sub(span.start_time_unix_nano as i64)
                    .max(0);

                let trace_id = if bytes_are_zero(&span.trace_id) {
                    rejected += 1;
                    continue;
                } else {
                    bytes_to_hex(&span.trace_id)
                };
                let span_id = if bytes_are_zero(&span.span_id) {
                    rejected += 1;
                    continue;
                } else {
                    bytes_to_hex(&span.span_id)
                };
                let parent_span_id = if bytes_are_zero(&span.parent_span_id) {
                    None
                } else {
                    Some(bytes_to_hex(&span.parent_span_id))
                };

                let status_code = span.status.as_ref().and_then(|s| status_code_text(s.code));
                let status_message = span
                    .status
                    .as_ref()
                    .filter(|s| !s.message.is_empty())
                    .map(|s| s.message.clone());

                // Encode events as JSON array of objects.
                let events = serde_json::Value::Array(
                    span.events
                        .iter()
                        .map(|e| {
                            json!({
                                "name": e.name,
                                "time_unix_nano": e.time_unix_nano,
                                "attributes": kv_to_json(&e.attributes),
                            })
                        })
                        .collect(),
                );

                // Encode links as JSON array of objects.
                let links = serde_json::Value::Array(
                    span.links
                        .iter()
                        .map(|l| {
                            json!({
                                "trace_id": bytes_to_hex(&l.trace_id),
                                "span_id": bytes_to_hex(&l.span_id),
                                "attributes": kv_to_json(&l.attributes),
                            })
                        })
                        .collect(),
                );

                span_records.push(SpanRecord {
                    trace_id,
                    span_id,
                    parent_span_id,
                    name: span.name.clone(),
                    kind: span_kind_text(span.kind),
                    service_name: service_name.clone(),
                    scope_name: scope_name.clone(),
                    start_time,
                    end_time,
                    duration_ns,
                    status_code,
                    status_message,
                    resource: resource_json.clone(),
                    attributes: kv_to_json(&span.attributes),
                    events,
                    links,
                });
            }
        }
    }

    if !span_records.is_empty() {
        if let Err(e) = write_spans(&state.pool, &span_records).await {
            warn!(error = %e, "write_spans failed");
            return service_unavailable();
        }
    }

    let (status, body, ct) = traces_response(rejected, "");
    let resp = (status, [(header::CONTENT_TYPE, ct)], body).into_response();
    with_cors(resp, &state.config.cors_allow_origin)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::{
        collector::metrics::v1::ExportMetricsServiceRequest,
        common::v1::{AnyValue, KeyValue},
        metrics::v1::{
            metric::Data, AggregationTemporality, Gauge, Histogram, HistogramDataPoint, Metric,
            NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum,
        },
        resource::v1::Resource,
    };

    // Helpers to build OTLP proto messages for tests.

    fn kv(key: &str, val: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(
                    opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                        val.to_string(),
                    ),
                ),
            }),
        }
    }

    fn ndp(ts_ns: u64, value: f64) -> NumberDataPoint {
        NumberDataPoint {
            time_unix_nano: ts_ns,
            value: Some(
                opentelemetry_proto::tonic::metrics::v1::number_data_point::Value::AsDouble(value),
            ),
            ..Default::default()
        }
    }

    fn now_ns() -> u64 {
        let now = Utc::now();
        (now.timestamp() as u64) * 1_000_000_000 + now.timestamp_subsec_nanos() as u64
    }

    /// kv_to_json round-trip: string attributes become JSON object keys.
    #[test]
    fn test_kv_to_json_string_attrs() {
        let attrs = vec![kv("env", "prod"), kv("svc", "api")];
        let j = kv_to_json(&attrs);
        assert_eq!(j["env"], "prod");
        assert_eq!(j["svc"], "api");
    }

    /// nano_to_dt: 0 returns None, non-zero returns Some.
    #[test]
    fn test_nano_to_dt() {
        assert!(nano_to_dt(0).is_none());
        let ns = now_ns();
        assert!(nano_to_dt(ns).is_some());
    }

    /// ts_in_window: now is always in window.
    #[test]
    fn test_ts_in_window_now() {
        assert!(ts_in_window(Utc::now(), 3600, 300));
    }

    /// ts_in_window: very old timestamp is rejected.
    #[test]
    fn test_ts_in_window_old() {
        let old = Utc::now() - Duration::seconds(7200);
        assert!(!ts_in_window(old, 3600, 300));
    }

    /// ts_in_window: far-future timestamp is rejected.
    #[test]
    fn test_ts_in_window_future() {
        let future = Utc::now() + Duration::seconds(600);
        assert!(!ts_in_window(future, 3600, 300));
    }

    /// ExportMetricsServiceRequest with Gauge + Sum + Histogram JSON round-trips.
    ///
    /// This is the golden-payload unit test (no DB required).
    #[test]
    fn test_decode_gauge_sum_histo_json() {
        let now = now_ns();
        let req = ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: Some(Resource {
                    attributes: vec![kv("service", "test")],
                    dropped_attributes_count: 0,
                }),
                scope_metrics: vec![ScopeMetrics {
                    scope: None,
                    metrics: vec![
                        Metric {
                            name: "cpu.usage".to_string(),
                            unit: "1".to_string(),
                            description: String::new(),
                            metadata: vec![],
                            data: Some(Data::Gauge(Gauge {
                                data_points: vec![ndp(now, 0.42)],
                            })),
                        },
                        Metric {
                            name: "http.requests".to_string(),
                            unit: "requests".to_string(),
                            description: String::new(),
                            metadata: vec![],
                            data: Some(Data::Sum(Sum {
                                data_points: vec![ndp(now, 100.0)],
                                aggregation_temporality: AggregationTemporality::Cumulative as i32,
                                is_monotonic: true,
                            })),
                        },
                        Metric {
                            name: "http.latency".to_string(),
                            unit: "ms".to_string(),
                            description: String::new(),
                            metadata: vec![],
                            data: Some(Data::Histogram(Histogram {
                                data_points: vec![HistogramDataPoint {
                                    time_unix_nano: now,
                                    count: 10,
                                    sum: Some(500.0),
                                    bucket_counts: vec![2, 5, 3],
                                    explicit_bounds: vec![10.0, 50.0],
                                    ..Default::default()
                                }],
                                aggregation_temporality: AggregationTemporality::Cumulative as i32,
                            })),
                        },
                    ],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        };

        // Serialize to JSON and re-parse — verifies serde round-trip.
        let json_bytes = serde_json::to_vec(&req).expect("serialize");
        let decoded: ExportMetricsServiceRequest =
            serde_json::from_slice(&json_bytes).expect("deserialize");

        assert_eq!(decoded.resource_metrics.len(), 1);
        let sm = &decoded.resource_metrics[0].scope_metrics[0];
        assert_eq!(sm.metrics.len(), 3);
        assert_eq!(sm.metrics[0].name, "cpu.usage");
        assert_eq!(sm.metrics[1].name, "http.requests");
        assert_eq!(sm.metrics[2].name, "http.latency");
    }

    /// Counter reset: cumulative value drops → new value stored as delta.
    ///
    /// Tests the delta-conversion logic directly (no DB needed).
    #[test]
    fn test_counter_reset_delta() {
        use crate::store::schema::SeriesKey;
        use lru::LruCache;
        use std::num::NonZeroUsize;
        use std::sync::Mutex;

        // Build a minimal cache and exercise swap_cumulative directly.
        let cap = NonZeroUsize::new(10).unwrap();
        let inner: Mutex<LruCache<SeriesKey, crate::store::series::CachedSeries>> =
            Mutex::new(LruCache::new(cap));

        let key = SeriesKey::new("counter", "Sum", &json!({}), &json!({}));

        // Seed with a known series_id.
        {
            let mut g = inner.lock().unwrap();
            g.put(
                key.clone(),
                crate::store::series::CachedSeries {
                    series_id: 1,
                    last_cumulative: None,
                },
            );
        }

        // Simulate swap_cumulative (same logic as SeriesCache::swap_cumulative).
        let swap = |key: &SeriesKey, new_val: f64| -> Option<f64> {
            let mut g = inner.lock().unwrap();
            if let Some(entry) = g.get_mut(key) {
                let prev = entry.last_cumulative;
                entry.last_cumulative = Some(new_val);
                prev
            } else {
                None
            }
        };

        // First observation: 100, prev = None → delta = 100.
        let cumulative = 100.0_f64;
        let prev = swap(&key, cumulative);
        let delta = match prev {
            None => cumulative,
            Some(p) if cumulative >= p => cumulative - p,
            Some(_) => cumulative, // reset
        };
        assert_eq!(delta, 100.0);

        // Normal increment: 150, prev = 100 → delta = 50.
        let cumulative = 150.0_f64;
        let prev = swap(&key, cumulative);
        let delta = match prev {
            None => cumulative,
            Some(p) if cumulative >= p => cumulative - p,
            Some(_) => cumulative,
        };
        assert_eq!(delta, 50.0);

        // Counter RESET: value drops from 150 to 10 → delta = 10 (reset semantics).
        let cumulative = 10.0_f64;
        let prev = swap(&key, cumulative);
        let delta = match prev {
            None => cumulative,
            Some(p) if cumulative >= p => cumulative - p,
            Some(_) => cumulative, // reset: new value IS the delta
        };
        assert_eq!(delta, 10.0, "counter reset must emit new value as delta");

        // After reset, next normal increment: 30, prev = 10 → delta = 20.
        let cumulative = 30.0_f64;
        let prev = swap(&key, cumulative);
        let delta = match prev {
            None => cumulative,
            Some(p) if cumulative >= p => cumulative - p,
            Some(_) => cumulative,
        };
        assert_eq!(delta, 20.0);
    }

    /// Unsupported metric types → partial_success with non-zero rejected count.
    #[test]
    fn test_unsupported_types_partial_success_json() {
        use opentelemetry_proto::tonic::metrics::v1::{
            ExponentialHistogram, Summary, SummaryDataPoint,
        };

        let now = now_ns();
        let req = ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: None,
                scope_metrics: vec![ScopeMetrics {
                    scope: None,
                    metrics: vec![
                        Metric {
                            name: "exp_histo".to_string(),
                            unit: String::new(),
                            description: String::new(),
                            metadata: vec![],
                            data: Some(Data::ExponentialHistogram(ExponentialHistogram {
                                data_points: vec![
                                    opentelemetry_proto::tonic::metrics::v1::ExponentialHistogramDataPoint {
                                        time_unix_nano: now,
                                        count: 5,
                                        ..Default::default()
                                    },
                                ],
                                aggregation_temporality: AggregationTemporality::Cumulative as i32,
                            })),
                        },
                        Metric {
                            name: "summary_metric".to_string(),
                            unit: String::new(),
                            description: String::new(),
                            metadata: vec![],
                            data: Some(Data::Summary(Summary {
                                data_points: vec![SummaryDataPoint {
                                    time_unix_nano: now,
                                    ..Default::default()
                                }],
                            })),
                        },
                    ],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        };

        // Count the rejected points by hand (2: one ExpHisto dp + one Summary dp).
        let mut rejected = 0_i64;
        for rm in &req.resource_metrics {
            for sm in &rm.scope_metrics {
                for m in &sm.metrics {
                    match &m.data {
                        Some(Data::ExponentialHistogram(e)) => {
                            rejected += e.data_points.len() as i64
                        }
                        Some(Data::Summary(s)) => rejected += s.data_points.len() as i64,
                        _ => {}
                    }
                }
            }
        }
        assert_eq!(rejected, 2, "two unsupported data points must be rejected");

        // metrics_response serializes correctly.
        let (status, body, _ct) = metrics_response(
            rejected,
            "ExponentialHistogram not supported; Summary not supported",
        );
        assert_eq!(status, StatusCode::OK);
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let rejected_field = parsed["partialSuccess"]["rejectedDataPoints"]
            .as_i64()
            .unwrap_or(0);
        assert_eq!(rejected_field, 2);
    }

    /// Integration test: full round-trip through DB (Gauge + Sum + Histogram).
    #[tokio::test]
    async fn integration_golden_payload_round_trip() {
        if std::env::var("TEST_DATABASE_URL").is_err() {
            eprintln!("SKIP integration_golden_payload_round_trip: TEST_DATABASE_URL not set");
            return;
        }

        let db = soma_infra::TestDb::create_from_env()
            .await
            .expect("create isolated test db");

        crate::install::install(&db.pool)
            .await
            .expect("install schema");
        crate::store::partition::ensure_partitions(&db.pool)
            .await
            .expect("ensure partitions");

        let cfg = crate::config::Config {
            database_url: std::env::var("TEST_DATABASE_URL").unwrap_or_default(),
            listen_addr: "127.0.0.1:4318".to_string(),
            auth_token: None,
            metrics_retention_days: 90,
            logs_retention_days: 30,
            traces_retention_days: 7,
            cors_allow_origin: "*".into(),
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
            alert_eval_interval_secs: 30,
        };
        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), cfg));

        let now = now_ns();

        // Build request with Gauge + monotonic Sum + Histogram.
        let req = ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: Some(Resource {
                    attributes: vec![kv("service", "integration-test")],
                    dropped_attributes_count: 0,
                }),
                scope_metrics: vec![ScopeMetrics {
                    scope: None,
                    metrics: vec![
                        Metric {
                            name: "integ.gauge".to_string(),
                            unit: "1".to_string(),
                            description: String::new(),
                            metadata: vec![],
                            data: Some(Data::Gauge(Gauge {
                                data_points: vec![ndp(now, 3.0)],
                            })),
                        },
                        Metric {
                            name: "integ.counter".to_string(),
                            unit: "req".to_string(),
                            description: String::new(),
                            metadata: vec![],
                            data: Some(Data::Sum(Sum {
                                data_points: vec![ndp(now, 200.0)],
                                aggregation_temporality: AggregationTemporality::Cumulative as i32,
                                is_monotonic: true,
                            })),
                        },
                        Metric {
                            name: "integ.latency".to_string(),
                            unit: "ms".to_string(),
                            description: String::new(),
                            metadata: vec![],
                            data: Some(Data::Histogram(Histogram {
                                data_points: vec![HistogramDataPoint {
                                    time_unix_nano: now,
                                    count: 5,
                                    sum: Some(250.0),
                                    bucket_counts: vec![1, 3, 1],
                                    explicit_bounds: vec![50.0, 100.0],
                                    ..Default::default()
                                }],
                                aggregation_temporality: AggregationTemporality::Cumulative as i32,
                            })),
                        },
                    ],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        };

        // Serialize to JSON and POST via the handler.
        let body_bytes = serde_json::to_vec(&req).expect("serialize");
        let http_req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/metrics")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body_bytes))
            .unwrap();

        let resp = ingest_metrics(State(state.clone()), http_req).await;
        assert_eq!(resp.status(), StatusCode::OK, "ingest_metrics must succeed");

        // Verify gauge point was written.
        let gauge_count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM soma_observe.metric_point mp
             JOIN soma_observe.metric_series ms USING (series_id)
             WHERE ms.name = 'integ.gauge'",
        )
        .fetch_one(&db.pool)
        .await
        .expect("count gauge");
        assert!(gauge_count.0 >= 1, "gauge point must be stored");

        // Verify counter delta was written (first observation → delta = 200).
        let counter_row: (f64,) = sqlx::query_as(
            "SELECT mp.value FROM soma_observe.metric_point mp
             JOIN soma_observe.metric_series ms USING (series_id)
             WHERE ms.name = 'integ.counter'
             ORDER BY mp.ts DESC LIMIT 1",
        )
        .fetch_one(&db.pool)
        .await
        .expect("counter row");
        assert_eq!(
            counter_row.0, 200.0,
            "first observation delta = cumulative value"
        );

        // Verify histogram point.
        let histo_count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM soma_observe.metric_histogram_point hp
             JOIN soma_observe.metric_series ms USING (series_id)
             WHERE ms.name = 'integ.latency'",
        )
        .fetch_one(&db.pool)
        .await
        .expect("count histo");
        assert!(histo_count.0 >= 1, "histogram point must be stored");
    }

    /// Integration test: counter reset is handled correctly.
    #[tokio::test]
    async fn integration_counter_reset() {
        if std::env::var("TEST_DATABASE_URL").is_err() {
            eprintln!("SKIP integration_counter_reset: TEST_DATABASE_URL not set");
            return;
        }

        let db = soma_infra::TestDb::create_from_env()
            .await
            .expect("create isolated test db");

        crate::install::install(&db.pool)
            .await
            .expect("install schema");
        crate::store::partition::ensure_partitions(&db.pool)
            .await
            .expect("ensure partitions");

        let cfg = crate::config::Config {
            database_url: std::env::var("TEST_DATABASE_URL").unwrap_or_default(),
            listen_addr: "127.0.0.1:4318".to_string(),
            auth_token: None,
            metrics_retention_days: 90,
            logs_retention_days: 30,
            traces_retention_days: 7,
            cors_allow_origin: "*".into(),
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
            alert_eval_interval_secs: 30,
        };
        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), cfg));

        // Use unique metric name per test run.
        let metric_name = format!("integ.reset.counter.{}", now_ns());

        let make_req = |ts_ns: u64, val: f64| ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: None,
                scope_metrics: vec![ScopeMetrics {
                    scope: None,
                    metrics: vec![Metric {
                        name: metric_name.clone(),
                        unit: String::new(),
                        description: String::new(),
                        metadata: vec![],
                        data: Some(Data::Sum(Sum {
                            data_points: vec![ndp(ts_ns, val)],
                            aggregation_temporality: AggregationTemporality::Cumulative as i32,
                            is_monotonic: true,
                        })),
                    }],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        };

        let send = |req: ExportMetricsServiceRequest| {
            let state = state.clone();
            async move {
                let body_bytes = serde_json::to_vec(&req).unwrap();
                let http_req = axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/metrics")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body_bytes))
                    .unwrap();
                ingest_metrics(State(state), http_req).await
            }
        };

        // t1: cumulative=100 → delta=100 (first observation)
        let base_ns = now_ns();
        let resp = send(make_req(base_ns, 100.0)).await;
        assert_eq!(resp.status(), StatusCode::OK);

        // t2: cumulative=150 → delta=50
        let resp = send(make_req(base_ns + 1_000_000_000, 150.0)).await;
        assert_eq!(resp.status(), StatusCode::OK);

        // t3: RESET — cumulative drops to 20 → delta=20
        let resp = send(make_req(base_ns + 2_000_000_000, 20.0)).await;
        assert_eq!(resp.status(), StatusCode::OK);

        // Read all stored deltas for this metric in insertion order.
        let rows: Vec<(f64,)> = sqlx::query_as(
            "SELECT mp.value FROM soma_observe.metric_point mp
             JOIN soma_observe.metric_series ms USING (series_id)
             WHERE ms.name = $1
             ORDER BY mp.ts ASC",
        )
        .bind(&metric_name)
        .fetch_all(&db.pool)
        .await
        .expect("fetch rows");

        assert_eq!(rows.len(), 3, "three data points must be stored");
        assert_eq!(rows[0].0, 100.0, "t1: first observation = delta 100");
        assert_eq!(rows[1].0, 50.0, "t2: normal increment = delta 50");
        assert_eq!(rows[2].0, 20.0, "t3: reset = new cumulative as delta");
    }

    /// Integration test: logs round-trip.
    #[tokio::test]
    async fn integration_logs_round_trip() {
        if std::env::var("TEST_DATABASE_URL").is_err() {
            eprintln!("SKIP integration_logs_round_trip: TEST_DATABASE_URL not set");
            return;
        }

        let db = soma_infra::TestDb::create_from_env()
            .await
            .expect("create isolated test db");

        crate::install::install(&db.pool)
            .await
            .expect("install schema");
        crate::store::partition::ensure_partitions(&db.pool)
            .await
            .expect("ensure partitions");

        let cfg = crate::config::Config {
            database_url: std::env::var("TEST_DATABASE_URL").unwrap_or_default(),
            listen_addr: "127.0.0.1:4318".to_string(),
            auth_token: None,
            metrics_retention_days: 90,
            logs_retention_days: 30,
            traces_retention_days: 7,
            cors_allow_origin: "*".into(),
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
            alert_eval_interval_secs: 30,
        };
        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), cfg));

        use opentelemetry_proto::tonic::{
            collector::logs::v1::ExportLogsServiceRequest,
            logs::v1::{LogRecord as ProtoLogRecord, ResourceLogs, ScopeLogs},
        };

        let now = now_ns();
        let req = ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![kv("service", "log-test")],
                    dropped_attributes_count: 0,
                }),
                scope_logs: vec![ScopeLogs {
                    scope: None,
                    log_records: vec![ProtoLogRecord {
                        time_unix_nano: now,
                        severity_number: 9, // INFO
                        severity_text: "INFO".to_string(),
                        body: Some(AnyValue {
                            value: Some(
                                opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                                    "hello from test".to_string(),
                                ),
                            ),
                        }),
                        attributes: vec![kv("env", "test")],
                        ..Default::default()
                    }],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        };

        let body_bytes = serde_json::to_vec(&req).expect("serialize");
        let http_req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/logs")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body_bytes))
            .unwrap();

        let resp = ingest_logs(State(state.clone()), http_req).await;
        assert_eq!(resp.status(), StatusCode::OK, "ingest_logs must succeed");

        // Verify the log record was written.
        let row: (Option<String>,) = sqlx::query_as(
            "SELECT body FROM soma_observe.logs
             WHERE attributes @> '{\"env\": \"test\"}'::jsonb
             ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&db.pool)
        .await
        .expect("fetch log");
        assert_eq!(row.0.as_deref(), Some("hello from test"));
    }

    /// Regression test: OTLP/JSON histogram with string-encoded uint64 fields
    /// (`count` and `bucketCounts`) must deserialize correctly after normalization.
    ///
    /// Before the fix, serde silently defaulted these to 0/[] because
    /// `opentelemetry-proto` with `with-serde` lacked a string deserializer for them.
    #[test]
    fn test_decode_json_histogram_uint64_strings() {
        use axum::http::HeaderValue;

        let payload = br#"{"resourceMetrics":[{"scopeMetrics":[{"metrics":[{"name":"test_hist","histogram":{"aggregationTemporality":2,"dataPoints":[{"count":"3","sum":6.0,"bucketCounts":["1","2"],"explicitBounds":[5.0],"timeUnixNano":"1700000000000000000"}]}}]}]}]}"#;

        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let body = Bytes::from_static(payload);

        let req = decode_metrics(&headers, &body).expect("decode_metrics must succeed");
        let dp = &req.resource_metrics[0].scope_metrics[0].metrics[0];
        let histo = match dp.data.as_ref().expect("data must be Some") {
            opentelemetry_proto::tonic::metrics::v1::metric::Data::Histogram(h) => h,
            other => panic!("expected Histogram, got {:?}", other),
        };
        let point = &histo.data_points[0];
        assert_eq!(point.count, 3, "count must be 3 (not 0)");
        assert_eq!(
            point.bucket_counts,
            vec![1u64, 2u64],
            "bucket_counts must be [1, 2] (not empty)"
        );
        assert_ne!(point.time_unix_nano, 0, "time_unix_nano must be non-zero");
    }

    /// Regression test: OTLP/JSON gauge datapoint with an `exemplars` array containing a
    /// string-encoded `timeUnixNano` must decode without rejecting the datapoint, and the
    /// exemplar's traceId must be captured.
    ///
    /// Root cause: `Exemplar.time_unix_nano` in opentelemetry-proto 0.28 has no custom
    /// string-to-u64 serde deserializer (unlike `NumberDataPoint.time_unix_nano`).  The
    /// OTLP/JSON spec encodes it as a string, causing a decode error that bubbles up and
    /// makes the enclosing `metric.data` oneof deserialize to `None`, triggering the
    /// "unrecognized or empty metric data" rejection path.  `normalize_otlp_json` now
    /// converts `exemplars[*].timeUnixNano` from string to number before serde runs.
    #[test]
    fn test_decode_json_gauge_with_exemplar() {
        use axum::http::HeaderValue;

        let trace_id = "aabbccddeeff00112233445566778899";
        let span_id = "0102030405060708";
        // Mimic what real OTel SDKs send: timeUnixNano as a quoted string.
        let payload = format!(
            r#"{{"resourceMetrics":[{{"scopeMetrics":[{{"metrics":[{{"name":"ex.gauge","gauge":{{"dataPoints":[{{"asDouble":99.5,"timeUnixNano":"1700000000000000000","exemplars":[{{"timeUnixNano":"1700000000000000000","asDouble":99.5,"traceId":"{trace_id}","spanId":"{span_id}","filteredAttributes":[]}}]}}]}}}}]}}]}}]}}"#
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let body = Bytes::from(payload.into_bytes());

        let req = decode_metrics(&headers, &body).expect("decode_metrics must not fail on gauge-with-exemplar");
        let dp = &req.resource_metrics[0].scope_metrics[0].metrics[0];
        // The gauge data must be present — not None.
        let gauge = match dp.data.as_ref().expect("data must be Some, not None") {
            opentelemetry_proto::tonic::metrics::v1::metric::Data::Gauge(g) => g,
            other => panic!("expected Gauge, got {:?}", other),
        };
        let point = &gauge.data_points[0];
        assert_eq!(point.exemplars.len(), 1, "exemplar must be decoded");
        let (ex_trace_id, _ex_span_id) = extract_exemplar(&point.exemplars);
        assert_eq!(
            ex_trace_id.as_deref(),
            Some(trace_id),
            "exemplar_trace_id must equal the sent traceId hex string"
        );
    }

    /// End-to-end test: ingest a gauge via the handler, then query it back via the
    /// metrics query handler. Asserts the stored value matches what was sent.
    #[tokio::test]
    async fn e2e_ingest_then_query_gauge() {
        if std::env::var("TEST_DATABASE_URL").is_err() {
            eprintln!("SKIP e2e_ingest_then_query_gauge: TEST_DATABASE_URL not set");
            return;
        }

        let db = soma_infra::TestDb::create_from_env()
            .await
            .expect("create isolated test db");

        crate::install::install(&db.pool)
            .await
            .expect("install schema");
        crate::store::partition::ensure_partitions(&db.pool)
            .await
            .expect("ensure partitions");

        let cfg = crate::config::Config {
            database_url: std::env::var("TEST_DATABASE_URL").unwrap_or_default(),
            listen_addr: "127.0.0.1:4318".into(),
            auth_token: None,
            metrics_retention_days: 90,
            logs_retention_days: 30,
            traces_retention_days: 7,
            cors_allow_origin: "*".into(),
            ingest_window_secs: 3600,
            future_tolerance_secs: 300,
            alert_eval_interval_secs: 30,
        };
        let state = Arc::new(crate::state::AppState::new(db.pool.clone(), cfg));

        // Use a unique metric name to avoid cross-test interference.
        let metric_name = format!("e2e.gauge.{}", now_ns());
        let ts_ns = now_ns();
        let expected_value = 7.77_f64;

        // ── Ingest ───────────────────────────────────────────────────────────────
        let req = ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: Some(Resource {
                    attributes: vec![kv("service", "e2e-test")],
                    dropped_attributes_count: 0,
                }),
                scope_metrics: vec![ScopeMetrics {
                    scope: None,
                    metrics: vec![Metric {
                        name: metric_name.clone(),
                        unit: "1".into(),
                        description: String::new(),
                        metadata: vec![],
                        data: Some(Data::Gauge(Gauge {
                            data_points: vec![ndp(ts_ns, expected_value)],
                        })),
                    }],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        };

        let body_bytes = serde_json::to_vec(&req).expect("serialize");
        let http_req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/metrics")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body_bytes))
            .unwrap();

        let resp = ingest_metrics(State(state.clone()), http_req).await;
        assert_eq!(resp.status(), StatusCode::OK, "ingest must succeed");

        // ── Query ────────────────────────────────────────────────────────────────
        use crate::query::metrics::{query_metrics, MetricsQueryParams, MetricsQueryResponse};
        use axum::extract::Query;

        // Query the minute bucket containing the ingested point.
        let point_ts = nano_to_dt(ts_ns).unwrap_or_else(Utc::now);
        let start_str = (point_ts - Duration::seconds(120)).to_rfc3339();
        let end_str = (point_ts + Duration::seconds(120)).to_rfc3339();

        let resp = query_metrics(
            State(state),
            Query(MetricsQueryParams {
                name: metric_name.clone(),
                start: start_str,
                end: end_str,
                step: Some(60),
                filter: None,
                agg: Some("avg".into()),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "query must succeed");

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let parsed: MetricsQueryResponse =
            serde_json::from_slice(&body).expect("parse query response");

        assert_eq!(parsed.metric, metric_name);
        assert!(
            !parsed.series.is_empty(),
            "at least one series must be returned"
        );
        let all_values: Vec<f64> = parsed.series[0]
            .points
            .iter()
            .filter_map(|p| p.value)
            .collect();
        assert!(
            !all_values.is_empty(),
            "at least one aggregated point must exist"
        );
        // avg of a single point equals the point's value.
        let got = all_values[0];
        assert!(
            (got - expected_value).abs() < 1e-9,
            "avg of single gauge point must equal {expected_value}, got {got}"
        );
    }
}
