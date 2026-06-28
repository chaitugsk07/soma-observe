//! Metrics page — browse metric names, series, and plot time-series data.

use crate::api::{query_metrics, get_metric_names, get_metric_series, MetricPoint, MetricSeries};
use crate::app::AppCtx;
use leptos::prelude::*;
use soma_ui::{
    Alert, AlertDescription, AlertTitle, AlertVariant, Badge, BadgeVariant, Button, ButtonSize,
    ButtonVariant, Column, DataTable, Empty, Input, LineChart, LineVariant, PageHeader, Select,
    SelectContent, SelectItem, Spinner, Table, TableBody, TableCell, TableHead, TableHeader,
    TableRow,
};
use soma_ui::ChartPoint;
use soma_ui::ChartSeries;
use std::collections::HashMap;

fn short_ts(ts: &str) -> String {
    // Extract HH:MM from ISO-8601 or unix-seconds string.
    // e.g. "2024-01-15T14:32:00Z" → "14:32"
    if ts.len() >= 16 && ts.contains('T') {
        ts[11..16].to_string()
    } else {
        ts.chars().take(5).collect()
    }
}

fn metric_series_label(s: &MetricSeries) -> String {
    // Best-effort short label from resource JSON.
    if let Some(obj) = s.resource.as_object() {
        if let Some(svc) = obj.get("service.name").and_then(|v| v.as_str()) {
            return svc.to_string();
        }
    }
    format!("series {}", s.series_id)
}

fn point_row(idx: usize, p: &MetricPoint) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("idx".to_string(), idx.to_string());
    m.insert("start".to_string(), p.start.clone());
    m.insert("end".to_string(), p.end.clone());
    m.insert(
        "value".to_string(),
        p.value.map(|v| format!("{:.4}", v)).unwrap_or_else(|| "—".to_string()),
    );
    m.insert(
        "count".to_string(),
        p.count.map(|c| c.to_string()).unwrap_or_else(|| "—".to_string()),
    );
    m
}

#[component]
pub fn MetricsPage() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("AppCtx required");

    // --- Metric name list ---
    let names: RwSignal<Vec<String>> = RwSignal::new(vec![]);
    let names_err: RwSignal<Option<String>> = RwSignal::new(None);
    let names_loading = RwSignal::new(true);

    Effect::new(move |_| {
        let token = ctx.token.get();
        names_loading.set(true);
        leptos::task::spawn_local(async move {
            match get_metric_names(&token).await {
                Ok(v) => names.set(v),
                Err(e) => names_err.set(Some(e.message)),
            }
            names_loading.set(false);
        });
    });

    // --- Query controls ---
    let selected_name = RwSignal::new(String::new());
    let start_val = RwSignal::new(String::new());
    let end_val = RwSignal::new(String::new());
    let step_val = RwSignal::new("60".to_string());
    let agg_val = RwSignal::new("avg".to_string());
    let filter_val = RwSignal::new(String::new());

    // --- Series list ---
    let series_list: RwSignal<Vec<MetricSeries>> = RwSignal::new(vec![]);
    let series_loading = RwSignal::new(false);
    let series_err: RwSignal<Option<String>> = RwSignal::new(None);

    // Load series when name changes
    let token_for_series = ctx.token;
    Effect::new(move |_| {
        let name = selected_name.get();
        if name.is_empty() {
            series_list.set(vec![]);
            return;
        }
        let token = token_for_series.get();
        series_loading.set(true);
        series_err.set(None);
        leptos::task::spawn_local(async move {
            match get_metric_series(&token, &name).await {
                Ok(v) => series_list.set(v),
                Err(e) => series_err.set(Some(e.message)),
            }
            series_loading.set(false);
        });
    });

    // --- Query result ---
    let chart_series: RwSignal<Vec<ChartSeries>> = RwSignal::new(vec![]);
    let raw_points: RwSignal<Vec<(String, Vec<MetricPoint>)>> = RwSignal::new(vec![]);
    let query_unit = RwSignal::new(String::new());
    let query_loading = RwSignal::new(false);
    let query_err: RwSignal<Option<String>> = RwSignal::new(None);
    let query_done = RwSignal::new(false);

    let token_for_query = ctx.token;
    let run_query = move |_| {
        let name = selected_name.get_untracked();
        if name.is_empty() {
            return;
        }
        let token = token_for_query.get_untracked();
        let start = start_val.get_untracked();
        let end = end_val.get_untracked();
        let step = step_val.get_untracked();
        let agg = agg_val.get_untracked();
        let filter = filter_val.get_untracked();

        // Default: last 1h from now
        let (s, e) = if start.is_empty() {
            let now = (js_sys::Date::now() / 1000.0) as i64;
            (format!("{}", now - 3600), format!("{}", now))
        } else {
            (start, end)
        };

        query_loading.set(true);
        query_err.set(None);
        query_done.set(false);
        leptos::task::spawn_local(async move {
            match query_metrics(
                &token,
                &name,
                Some(&s),
                if e.is_empty() { None } else { Some(&e) },
                if step.is_empty() { None } else { Some(&step) },
                if filter.is_empty() { None } else { Some(&filter) },
                if agg.is_empty() { None } else { Some(&agg) },
            )
            .await
            {
                Ok(resp) => {
                    query_unit.set(resp.unit.clone().unwrap_or_default());
                    // Build chart series (cap at 6)
                    let capped: Vec<_> = resp.series.iter().take(6).collect();
                    let cs: Vec<ChartSeries> = capped
                        .iter()
                        .map(|qs| ChartSeries {
                            points: qs
                                .points
                                .iter()
                                .map(|p| ChartPoint {
                                    label: short_ts(&p.start),
                                    value: p.value.unwrap_or(0.0),
                                })
                                .collect(),
                        })
                        .collect();
                    chart_series.set(cs);
                    // raw points for table: flatten with series label
                    let rp: Vec<(String, Vec<MetricPoint>)> = resp
                        .series
                        .into_iter()
                        .enumerate()
                        .map(|(i, qs)| {
                            let label = format!("series {}", qs.series_id);
                            let _ = i;
                            (label, qs.points)
                        })
                        .collect();
                    raw_points.set(rp);
                }
                Err(e) => query_err.set(Some(e.message)),
            }
            query_loading.set(false);
            query_done.set(true);
        });
    };

    view! {
        <div class="space-y-6">
            <PageHeader title="Metrics".to_string()>
                <span class="text-xs text-muted-foreground">"Browse metric names and series"</span>
            </PageHeader>

            // Error: names failed
            {move || names_err.get().map(|msg| view! {
                <Alert variant=AlertVariant::Destructive>
                    <AlertTitle>"Failed to load metric names"</AlertTitle>
                    <AlertDescription>{msg}</AlertDescription>
                </Alert>
            })}

            // Name selector + query controls
            <div class="flex items-end gap-3 flex-wrap">
                <div class="w-64">
                    <label class="text-xs text-muted-foreground mb-1 block">"Metric name"</label>
                    {move || {
                        if names_loading.get() {
                            return view! { <div class="flex items-center gap-2 text-sm text-muted-foreground"><Spinner />"Loading…"</div> }.into_any();
                        }
                        let opts: Vec<(String, String)> = names.get()
                            .into_iter()
                            .map(|n| (n.clone(), n))
                            .collect();
                        if opts.is_empty() {
                            return view! { <span class="text-sm text-muted-foreground">"No metrics ingested yet"</span> }.into_any();
                        }
                        view! {
                            <Select value=selected_name placeholder="Select metric…".to_string()>
                                <SelectContent>
                                    <For
                                        each=move || names.get()
                                        key=|n| n.clone()
                                        children=move |n| {
                                            let v = n.clone();
                                            view! {
                                                <SelectItem value=v.clone()>{v}</SelectItem>
                                            }
                                        }
                                    />
                                </SelectContent>
                            </Select>
                        }.into_any()
                    }}
                </div>
                <div class="w-40">
                    <label class="text-xs text-muted-foreground mb-1 block">"Start (unix or RFC3339)"</label>
                    <Input value=start_val placeholder="default: -1h".to_string() />
                </div>
                <div class="w-40">
                    <label class="text-xs text-muted-foreground mb-1 block">"End"</label>
                    <Input value=end_val placeholder="default: now".to_string() />
                </div>
                <div class="w-24">
                    <label class="text-xs text-muted-foreground mb-1 block">"Step (s)"</label>
                    <Input value=step_val placeholder="60".to_string() />
                </div>
                <div class="w-32">
                    <label class="text-xs text-muted-foreground mb-1 block">"Aggregation"</label>
                    <Select value=agg_val placeholder="avg".to_string()>
                        <SelectContent>
                            <SelectItem value="avg".to_string()>"avg"</SelectItem>
                            <SelectItem value="sum".to_string()>"sum"</SelectItem>
                            <SelectItem value="min".to_string()>"min"</SelectItem>
                            <SelectItem value="max".to_string()>"max"</SelectItem>
                            <SelectItem value="count".to_string()>"count"</SelectItem>
                        </SelectContent>
                    </Select>
                </div>
                <div class="w-48">
                    <label class="text-xs text-muted-foreground mb-1 block">"Attribute filter"</label>
                    <Input value=filter_val placeholder="key=value".to_string() />
                </div>
                <Button
                    variant=ButtonVariant::Default
                    size=ButtonSize::Sm
                    on:click=run_query
                >
                    "Query"
                </Button>
            </div>

            // Query error
            {move || query_err.get().map(|msg| view! {
                <Alert variant=AlertVariant::Destructive>
                    <AlertTitle>"Query failed"</AlertTitle>
                    <AlertDescription>{msg}</AlertDescription>
                </Alert>
            })}

            // Chart
            {move || {
                if query_loading.get() {
                    return view! { <div class="flex justify-center py-8"><Spinner /></div> }.into_any();
                }
                if !query_done.get() {
                    return ().into_any();
                }
                let cs = chart_series.get();
                if cs.is_empty() {
                    return view! {
                        <Empty title="No data".to_string() description="No metric points returned for this query.".to_string() />
                    }.into_any();
                }
                let unit = query_unit.get();
                let total_series = raw_points.get().len();
                let capped = total_series > 6;
                view! {
                    <div class="space-y-4">
                        <div class="rounded-lg border border-border bg-card p-4">
                            <div class="flex items-center justify-between mb-2">
                                <span class="text-sm font-medium text-foreground">
                                    {move || selected_name.get()}
                                </span>
                                {(!unit.is_empty()).then(|| view! {
                                    <Badge variant=BadgeVariant::Secondary>{unit.clone()}</Badge>
                                })}
                            </div>
                            {capped.then(|| view! {
                                <p class="text-xs text-muted-foreground mb-2">
                                    {format!("Showing 6 of {} series", total_series)}
                                </p>
                            })}
                            <LineChart
                                data=vec![]
                                variant=LineVariant::Multiple
                                series=cs
                            />
                        </div>

                        // Raw points table
                        <div class="space-y-3">
                            <For
                                each=move || raw_points.get()
                                key=|(label, _)| label.clone()
                                children=move |(label, points)| {
                                    let cols = vec![
                                        Column { key: "idx".to_string(), header: "#".to_string(), sortable: false, editable: false },
                                        Column { key: "start".to_string(), header: "Start".to_string(), sortable: false, editable: false },
                                        Column { key: "end".to_string(), header: "End".to_string(), sortable: false, editable: false },
                                        Column { key: "value".to_string(), header: "Value".to_string(), sortable: true, editable: false },
                                        Column { key: "count".to_string(), header: "Count".to_string(), sortable: false, editable: false },
                                    ];
                                    let rows: Vec<HashMap<String, String>> = points
                                        .iter()
                                        .enumerate()
                                        .map(|(i, p)| point_row(i + 1, p))
                                        .collect();
                                    view! {
                                        <div>
                                            <p class="text-xs font-mono text-muted-foreground mb-1">{label}</p>
                                            <DataTable columns=cols rows=rows page_size=20 />
                                        </div>
                                    }
                                }
                            />
                        </div>
                    </div>
                }.into_any()
            }}

            // Series list (below query)
            {move || {
                if selected_name.get().is_empty() {
                    return ().into_any();
                }
                let sl = series_list.get();
                if series_loading.get() {
                    return view! { <div class="flex items-center gap-2 text-sm text-muted-foreground"><Spinner />"Loading series…"</div> }.into_any();
                }
                if let Some(msg) = series_err.get() {
                    return view! {
                        <Alert variant=AlertVariant::Destructive>
                            <AlertTitle>"Failed to load series"</AlertTitle>
                            <AlertDescription>{msg}</AlertDescription>
                        </Alert>
                    }.into_any();
                }
                if sl.is_empty() {
                    return view! {
                        <Empty title="No series".to_string() description="No series registered for this metric.".to_string() />
                    }.into_any();
                }
                view! {
                    <div>
                        <p class="text-sm font-medium mb-2">"Series"</p>
                        <Table>
                            <TableHeader>
                                <TableRow>
                                    <TableHead>"ID"</TableHead>
                                    <TableHead>"Kind"</TableHead>
                                    <TableHead>"Unit"</TableHead>
                                    <TableHead>"Resource"</TableHead>
                                </TableRow>
                            </TableHeader>
                            <TableBody>
                                <For
                                    each=move || sl.clone()
                                    key=|s| s.series_id
                                    children=|s| {
                                        let label = metric_series_label(&s);
                                        view! {
                                            <TableRow>
                                                <TableCell class="font-mono text-xs".to_string()>{s.series_id}</TableCell>
                                                <TableCell>
                                                    <Badge variant=BadgeVariant::Secondary>{s.kind}</Badge>
                                                </TableCell>
                                                <TableCell class="text-xs".to_string()>
                                                    {s.unit.as_deref().filter(|u| !u.is_empty()).unwrap_or("—").to_string()}
                                                </TableCell>
                                                <TableCell class="text-xs text-muted-foreground".to_string()>
                                                    {label}
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
        </div>
    }
}
