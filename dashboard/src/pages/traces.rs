//! Traces page — query traces and view waterfall details.

use crate::api::{get_trace, query_traces, SpanDetail, TraceSummary};
use crate::app::AppCtx;
use crate::util::relative_time;
use leptos::prelude::*;
use soma_ui::{
    Alert, AlertDescription, AlertTitle, AlertVariant, Badge, BadgeVariant, Button, ButtonSize,
    ButtonVariant, Card, CardContent, CardHeader, CardTitle, Empty, Input, PageHeader, Select,
    SelectContent, SelectItem, Spinner, Table, TableBody, TableCell, TableHead, TableHeader,
    TableRow,
};
use std::collections::HashMap;

// ── Colour palette for services (stable hash → index) ────────────────────────

/// Fixed hex palette for waterfall bar colors. Inline style avoids Tailwind purge.
const SERVICE_COLORS_HEX: &[&str] = &[
    "#3b82f6", // blue
    "#8b5cf6", // violet
    "#10b981", // emerald
    "#f59e0b", // amber
    "#ec4899", // pink
    "#14b8a6", // teal
    "#f43f5e", // rose
    "#6366f1", // indigo
];

fn service_color_hex(service: &str) -> &'static str {
    // djb2-style hash → palette index
    let hash: usize = service
        .bytes()
        .fold(5381usize, |h, b| h.wrapping_mul(33).wrapping_add(b as usize));
    SERVICE_COLORS_HEX[hash % SERVICE_COLORS_HEX.len()]
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn fmt_duration_ms(ms: i64) -> String {
    if ms < 1000 {
        format!("{}ms", ms)
    } else {
        format!("{:.2}s", ms as f64 / 1000.0)
    }
}

fn fmt_duration_ns(ns: i64) -> String {
    if ns < 1_000 {
        format!("{}ns", ns)
    } else if ns < 1_000_000 {
        format!("{:.1}µs", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.2}ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.2}s", ns as f64 / 1_000_000_000.0)
    }
}

/// Parse an ISO-8601 timestamp to milliseconds via JS Date.parse.
fn iso_to_ms(iso: &str) -> f64 {
    js_sys::Date::parse(iso)
}

// ── Waterfall ─────────────────────────────────────────────────────────────────

/// For each span, compute its depth (0 = root) by walking the parent chain.
fn compute_depths(spans: &[SpanDetail]) -> HashMap<String, usize> {
    // Build span_id → parent_span_id map
    let parent_of: HashMap<&str, &str> = spans
        .iter()
        .filter_map(|s| {
            s.parent_span_id
                .as_deref()
                .map(|p| (s.span_id.as_str(), p))
        })
        .collect();

    // For each span, walk up the chain counting hops.
    let mut depths = HashMap::new();
    for span in spans {
        let mut depth = 0usize;
        let mut cur = span.span_id.as_str();
        let mut visited = std::collections::HashSet::new();
        while let Some(&p) = parent_of.get(cur) {
            if !visited.insert(cur) {
                break; // cycle guard
            }
            depth += 1;
            cur = p;
        }
        depths.insert(span.span_id.clone(), depth);
    }
    depths
}

#[component]
fn Waterfall(spans: Vec<SpanDetail>) -> impl IntoView {
    if spans.is_empty() {
        return view! {
            <Empty
                title="No spans".to_string()
                description="This trace has no recorded spans.".to_string()
            />
        }
        .into_any();
    }

    // Compute time bounds (milliseconds since epoch).
    let start_ms = spans
        .iter()
        .map(|s| iso_to_ms(&s.start_time))
        .fold(f64::INFINITY, f64::min);
    let end_ms = spans
        .iter()
        .map(|s| iso_to_ms(&s.end_time))
        .fold(f64::NEG_INFINITY, f64::max);
    let trace_span_ms = (end_ms - start_ms).max(1.0);

    let depths = compute_depths(&spans);

    // Sort by start_time ascending (already ordered by server, but be safe).
    let mut sorted = spans.clone();
    sorted.sort_by(|a, b| {
        iso_to_ms(&a.start_time)
            .partial_cmp(&iso_to_ms(&b.start_time))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let rows: Vec<_> = sorted
        .iter()
        .map(|span| {
            let depth = *depths.get(&span.span_id).unwrap_or(&0);
            let span_start_ms = iso_to_ms(&span.start_time);
            let span_end_ms = iso_to_ms(&span.end_time);
            let left_pct = ((span_start_ms - start_ms) / trace_span_ms * 100.0).max(0.0);
            let width_pct = (((span_end_ms - span_start_ms) / trace_span_ms) * 100.0)
                .max(0.5)
                .min(100.0 - left_pct);

            let is_error = span
                .status_code
                .as_deref()
                .map(|s| s.eq_ignore_ascii_case("error"))
                .unwrap_or(false);

            let svc = span.service_name.as_deref().unwrap_or("");
            let bg_hex = if is_error { "#ef4444" } else { service_color_hex(svc) };
            let bar_style = if is_error {
                format!(
                    "left:{left_pct:.2}%;width:{width_pct:.2}%;background-color:{bg_hex};box-shadow:0 0 0 1px #f87171;"
                )
            } else {
                format!("left:{left_pct:.2}%;width:{width_pct:.2}%;background-color:{bg_hex};")
            };

            let service_label = if svc.is_empty() {
                None
            } else {
                Some(svc.to_string())
            };
            let dur_text = fmt_duration_ns(span.duration_ns);
            let span_name = span.name.clone();
            let title_attr = format!(
                "{} ({}) [{}]",
                span.name,
                svc,
                span.span_id
            );

            (
                depth,
                span_name,
                service_label,
                dur_text,
                bar_style,
                is_error,
                title_attr,
            )
        })
        .collect();

    view! {
        <div class="space-y-1 font-mono text-xs">
            {rows.into_iter().map(|(depth, name, svc, dur, bar_style, is_error, title)| {
                let indent = depth * 16; // px
                view! {
                    <div class="flex items-center gap-2 min-h-[28px]" title=title>
                        // Left label column
                        <div
                            class="flex-none w-64 flex items-center gap-1 overflow-hidden"
                            style=format!("padding-left:{}px", indent)
                        >
                            <span class=if is_error { "text-red-500 truncate font-semibold" } else { "text-foreground truncate" }>
                                {name}
                            </span>
                            {svc.map(|s| view! {
                                <span class="text-muted-foreground truncate">{format!("({})", s)}</span>
                            })}
                        </div>
                        // Bar column
                        <div class="flex-1 relative h-5 bg-muted/30 rounded overflow-hidden">
                            <div
                                class="h-full rounded"
                                style=bar_style
                            />
                        </div>
                        // Duration label
                        <div class="flex-none w-20 text-right text-muted-foreground">
                            {dur}
                        </div>
                    </div>
                }
            }).collect::<Vec<_>>()}
        </div>
    }
    .into_any()
}

// ── TracesPage ────────────────────────────────────────────────────────────────

#[component]
pub fn TracesPage() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("AppCtx required");

    // Filter signals
    let service_val = RwSignal::new(String::new());
    let name_val = RwSignal::new(String::new());
    let status_val = RwSignal::new(String::new());
    let min_dur_val = RwSignal::new(String::new());
    let max_dur_val = RwSignal::new(String::new());
    let start_val = RwSignal::new(String::new());
    let end_val = RwSignal::new(String::new());
    let limit_val = RwSignal::new("50".to_string());

    // Results
    let traces: RwSignal<Vec<TraceSummary>> = RwSignal::new(vec![]);
    let loading = RwSignal::new(false);
    let err: RwSignal<Option<String>> = RwSignal::new(None);
    let query_done = RwSignal::new(false);

    // Selected trace waterfall
    let selected_trace_id: RwSignal<Option<String>> = RwSignal::new(None);
    let span_details: RwSignal<Vec<SpanDetail>> = RwSignal::new(vec![]);
    let spans_loading = RwSignal::new(false);
    let spans_err: RwSignal<Option<String>> = RwSignal::new(None);

    let token_sig = ctx.token;

    let do_query = move |_| {
        let token = token_sig.get_untracked();
        let service = service_val.get_untracked();
        let name = name_val.get_untracked();
        let status = status_val.get_untracked();
        let min_dur = min_dur_val.get_untracked();
        let max_dur = max_dur_val.get_untracked();
        let start = start_val.get_untracked();
        let end = end_val.get_untracked();
        let limit_s = limit_val.get_untracked();
        let limit = limit_s.parse::<u32>().unwrap_or(50);

        // Default: last 1h
        let (s, e) = if start.is_empty() {
            let now = (js_sys::Date::now() / 1000.0) as i64;
            (format!("{}", now - 3600), format!("{}", now))
        } else {
            (start, end)
        };

        loading.set(true);
        err.set(None);
        query_done.set(false);
        selected_trace_id.set(None);
        span_details.set(vec![]);

        leptos::task::spawn_local(async move {
            match query_traces(
                &token,
                if service.is_empty() { None } else { Some(&service) },
                if name.is_empty() { None } else { Some(&name) },
                if status.is_empty() { None } else { Some(&status) },
                if min_dur.is_empty() { None } else { Some(&min_dur) },
                if max_dur.is_empty() { None } else { Some(&max_dur) },
                Some(&s),
                if e.is_empty() { None } else { Some(&e) },
                Some(limit),
            )
            .await
            {
                Ok(t) => traces.set(t),
                Err(e) => err.set(Some(e.message)),
            }
            loading.set(false);
            query_done.set(true);
        });
    };

    // Load spans when a trace row is clicked.
    let on_row_click = move |trace_id: String| {
        let token = token_sig.get_untracked();
        selected_trace_id.set(Some(trace_id.clone()));
        spans_loading.set(true);
        spans_err.set(None);
        span_details.set(vec![]);

        leptos::task::spawn_local(async move {
            match get_trace(&token, &trace_id).await {
                Ok(spans) => span_details.set(spans),
                Err(e) => spans_err.set(Some(e.message)),
            }
            spans_loading.set(false);
        });
    };

    view! {
        <div class="space-y-6">
            <PageHeader title="Traces".to_string()>
                <Button
                    variant=ButtonVariant::Default
                    size=ButtonSize::Sm
                    on:click=do_query
                >
                    "Query"
                </Button>
            </PageHeader>

            // Filters row
            <div class="flex items-end gap-3 flex-wrap">
                <div class="w-36">
                    <label class="text-xs text-muted-foreground mb-1 block">"Service"</label>
                    <Input value=service_val placeholder="any".to_string() />
                </div>
                <div class="w-36">
                    <label class="text-xs text-muted-foreground mb-1 block">"Root name"</label>
                    <Input value=name_val placeholder="any".to_string() />
                </div>
                <div class="w-32">
                    <label class="text-xs text-muted-foreground mb-1 block">"Status"</label>
                    <Select value=status_val placeholder="Any".to_string()>
                        <SelectContent>
                            <SelectItem value="".to_string()>"Any"</SelectItem>
                            <SelectItem value="ok".to_string()>"ok"</SelectItem>
                            <SelectItem value="error".to_string()>"error"</SelectItem>
                        </SelectContent>
                    </Select>
                </div>
                <div class="w-28">
                    <label class="text-xs text-muted-foreground mb-1 block">"Min dur (ms)"</label>
                    <Input value=min_dur_val placeholder="0".to_string() />
                </div>
                <div class="w-28">
                    <label class="text-xs text-muted-foreground mb-1 block">"Max dur (ms)"</label>
                    <Input value=max_dur_val placeholder="∞".to_string() />
                </div>
                <div class="w-40">
                    <label class="text-xs text-muted-foreground mb-1 block">"Start (unix or RFC3339)"</label>
                    <Input value=start_val placeholder="default: -1h".to_string() />
                </div>
                <div class="w-40">
                    <label class="text-xs text-muted-foreground mb-1 block">"End"</label>
                    <Input value=end_val placeholder="default: now".to_string() />
                </div>
                <div class="w-20">
                    <label class="text-xs text-muted-foreground mb-1 block">"Limit"</label>
                    <Input value=limit_val placeholder="50".to_string() />
                </div>
                <Button
                    variant=ButtonVariant::Outline
                    size=ButtonSize::Sm
                    on:click=do_query
                >
                    "Apply"
                </Button>
            </div>

            // Error banner
            {move || err.get().map(|msg| view! {
                <Alert variant=AlertVariant::Destructive>
                    <AlertTitle>"Query failed"</AlertTitle>
                    <AlertDescription>{msg}</AlertDescription>
                </Alert>
            })}

            // Trace list
            {move || {
                if loading.get() {
                    return view! { <div class="flex justify-center py-8"><Spinner /></div> }.into_any();
                }
                if !query_done.get() {
                    return view! {
                        <p class="text-sm text-muted-foreground">"Set filters and click Query to load traces."</p>
                    }.into_any();
                }
                let trace_list = traces.get();
                if trace_list.is_empty() {
                    return view! {
                        <Empty
                            title="No traces".to_string()
                            description="No traces matched the current filters.".to_string()
                        />
                    }.into_any();
                }
                let count = trace_list.len();
                let sel = selected_trace_id.get();
                view! {
                    <div class="space-y-2">
                        <p class="text-xs text-muted-foreground">{format!("{} traces", count)}</p>
                        <Table>
                            <TableHeader>
                                <TableRow>
                                    <TableHead>"Root"</TableHead>
                                    <TableHead class="w-36".to_string()>"Start"</TableHead>
                                    <TableHead class="w-24".to_string()>"Duration"</TableHead>
                                    <TableHead class="w-16".to_string()>"Spans"</TableHead>
                                    <TableHead class="w-20".to_string()>"Status"</TableHead>
                                </TableRow>
                            </TableHeader>
                            <TableBody>
                                <For
                                    each=move || traces.get()
                                    key=|t| t.trace_id.clone()
                                    children=move |trace| {
                                        let tid = trace.trace_id.clone();
                                        let tid2 = tid.clone();
                                        let is_sel = sel.as_deref() == Some(&tid);
                                        let row_class = if is_sel {
                                            "cursor-pointer bg-accent/50".to_string()
                                        } else {
                                            "cursor-pointer hover:bg-muted/40".to_string()
                                        };
                                        let rel = relative_time(&trace.start_time);
                                        let dur = fmt_duration_ms(trace.duration_ms);
                                        let svc = trace.root_service.clone();
                                        let root_name = trace.root_name.clone();
                                        let span_count = trace.span_count;
                                        let status = trace.status.clone();
                                        let status_variant = if status == "error" {
                                            BadgeVariant::Destructive
                                        } else {
                                            BadgeVariant::Success
                                        };
                                        view! {
                                            <TableRow
                                                class=row_class
                                                on:click=move |_| on_row_click(tid2.clone())
                                            >
                                                <TableCell>
                                                    <div class="flex flex-col">
                                                        <span class="text-sm font-medium">{root_name}</span>
                                                        {svc.map(|s| view! {
                                                            <span class="text-xs text-muted-foreground">{s}</span>
                                                        })}
                                                    </div>
                                                </TableCell>
                                                <TableCell class="text-xs text-muted-foreground whitespace-nowrap".to_string()>
                                                    {rel}
                                                </TableCell>
                                                <TableCell class="text-xs tabular-nums".to_string()>
                                                    {dur}
                                                </TableCell>
                                                <TableCell class="text-xs tabular-nums".to_string()>
                                                    {span_count.to_string()}
                                                </TableCell>
                                                <TableCell>
                                                    <Badge variant=status_variant>{status}</Badge>
                                                </TableCell>
                                            </TableRow>
                                        }
                                    }
                                />
                            </TableBody>
                        </Table>
                    </div>
                }.into_any()
            }}

            // Waterfall section — shown when a trace is selected
            {move || {
                let Some(tid) = selected_trace_id.get() else {
                    return ().into_any();
                };
                view! {
                    <Card>
                        <CardHeader>
                            <CardTitle>
                                {format!("Trace {}", &tid[..tid.len().min(16)])}
                            </CardTitle>
                        </CardHeader>
                        <CardContent>
                            {move || {
                                if spans_loading.get() {
                                    return view! {
                                        <div class="flex justify-center py-6"><Spinner /></div>
                                    }.into_any();
                                }
                                if let Some(e) = spans_err.get() {
                                    return view! {
                                        <Alert variant=AlertVariant::Destructive>
                                            <AlertTitle>"Failed to load spans"</AlertTitle>
                                            <AlertDescription>{e}</AlertDescription>
                                        </Alert>
                                    }.into_any();
                                }
                                let spans = span_details.get();
                                view! {
                                    <Waterfall spans=spans />
                                }.into_any()
                            }}
                        </CardContent>
                    </Card>
                }.into_any()
            }}
        </div>
    }
}
