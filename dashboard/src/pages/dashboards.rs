//! Dashboards page — savable named collections of metric panels.
//!
//! ponytail: layout is a simple CSS grid (Tailwind grid-cols-2); no drag/resize.
//! ponytail: Heatmap panel type is out of scope for v1 — it needs bucket×time
//!   histogram data that requires a separate query shape; follow-up work.

use crate::api::{
    create_dashboard, delete_dashboard, get_metric_names, list_dashboards, query_metrics,
    update_dashboard, Dashboard, DashboardSummary, Panel,
};
use crate::app::AppCtx;
use leptos::prelude::*;
use soma_ui::{
    Alert, AlertDescription, AlertTitle, AlertVariant, AreaChart, AreaVariant, BarChart,
    BarVariant, Button, ButtonSize, ButtonVariant, Card, CardContent, CardHeader, CardTitle,
    ChartPoint, Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle, Empty, Input,
    LineChart, LineVariant, PageHeader, Select, SelectContent, SelectItem, Sparkline, SparkVariant,
    Spinner, StatTile,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// "2024-01-15T14:32:00Z" → "14:32"
fn short_ts(ts: &str) -> String {
    if ts.len() >= 16 && ts.contains('T') {
        ts[11..16].to_string()
    } else {
        ts.chars().take(5).collect()
    }
}

/// Convert panel `range` string to seconds offset from now.
fn range_secs(range: &str) -> i64 {
    match range {
        "15m" => 900,
        "1h" => 3600,
        "6h" => 21600,
        "24h" => 86400,
        _ => 3600,
    }
}

// ── Single panel card ─────────────────────────────────────────────────────────

#[component]
fn PanelCard(panel: Panel, on_remove: Callback<()>) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("AppCtx required");

    let pts: RwSignal<Vec<ChartPoint>> = RwSignal::new(vec![]);
    let loading = RwSignal::new(true);
    let err: RwSignal<Option<String>> = RwSignal::new(None);

    // Fetch metric data on mount.
    let panel_clone = panel.clone();
    Effect::new(move |_| {
        let token = ctx.token.get();
        let p = panel_clone.clone();
        loading.set(true);
        err.set(None);
        leptos::task::spawn_local(async move {
            let now = (js_sys::Date::now() / 1000.0) as i64;
            let start = format!("{}", now - range_secs(&p.range));
            let end = format!("{}", now);
            match query_metrics(
                &token,
                &p.metric,
                Some(&start),
                Some(&end),
                None,
                None,
                Some(&p.agg),
            )
            .await
            {
                Ok(resp) => {
                    // Flatten all series points into one Vec<ChartPoint>, take first series.
                    let chart_pts: Vec<ChartPoint> = resp
                        .series
                        .into_iter()
                        .next()
                        .map(|s| {
                            s.points
                                .iter()
                                .map(|mp| ChartPoint {
                                    label: short_ts(&mp.start),
                                    value: mp.value.unwrap_or(0.0),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    pts.set(chart_pts);
                }
                Err(e) => err.set(Some(e.message)),
            }
            loading.set(false);
        });
    });

    let title = panel.title.clone();
    let title2 = title.clone(); // for use inside the inner closure
    let chart_type = panel.chart_type.clone();

    view! {
        <Card class="flex flex-col".to_string()>
            <CardHeader>
                <div class="flex items-center justify-between">
                    <CardTitle>{title.clone()}</CardTitle>
                    <Button
                        variant=ButtonVariant::Ghost
                        size=ButtonSize::Sm
                        on:click=move |_| on_remove.run(())
                    >
                        "\u{00d7}"
                    </Button>
                </div>
            </CardHeader>
            <CardContent>
                {move || {
                    if loading.get() {
                        return view! { <div class="flex justify-center py-4"><Spinner /></div> }.into_any();
                    }
                    if let Some(msg) = err.get() {
                        return view! {
                            <Alert variant=AlertVariant::Destructive>
                                <AlertDescription>{msg}</AlertDescription>
                            </Alert>
                        }.into_any();
                    }
                    let data = pts.get();
                    if data.is_empty() {
                        return view! {
                            <Empty title="No data".to_string() description=format!("No points for {} in range", panel.metric) />
                        }.into_any();
                    }

                    // Compute latest value and delta for stat/sparkline variants.
                    let latest_val = data.last().map(|p| p.value).unwrap_or(0.0);
                    let first_val = data.first().map(|p| p.value).unwrap_or(0.0);
                    let delta_pct = if first_val != 0.0 {
                        (latest_val - first_val) / first_val * 100.0
                    } else {
                        0.0
                    };
                    let has_delta = first_val != 0.0;
                    let latest_str = format!("{:.4}", latest_val);

                    match chart_type.as_str() {
                        "area" => view! {
                            <AreaChart data=data variant=AreaVariant::Default />
                        }.into_any(),
                        "bar" => view! {
                            <BarChart data=data variant=BarVariant::Default />
                        }.into_any(),
                        "sparkline" => view! {
                            <div class="h-12">
                                <Sparkline data=data variant=SparkVariant::Line />
                            </div>
                        }.into_any(),
                        "stat" => {
                            // #[prop(optional)] means pass f64 directly (macro wraps in Some).
                            // Skip delta prop when first_val is zero to avoid 0% noise.
                            if has_delta {
                                view! {
                                    <StatTile
                                        label=title2.clone()
                                        value=latest_str
                                        delta=delta_pct
                                        spark=data
                                    />
                                }.into_any()
                            } else {
                                view! {
                                    <StatTile
                                        label=title2.clone()
                                        value=latest_str
                                        spark=data
                                    />
                                }.into_any()
                            }
                        },
                        // "line" or anything else
                        _ => view! {
                            <LineChart data=vec![] variant=LineVariant::Default
                                series=vec![soma_ui::ChartSeries { points: data }] />
                        }.into_any(),
                    }
                }}
            </CardContent>
        </Card>
    }
}

// ── Add panel dialog ──────────────────────────────────────────────────────────

#[component]
fn AddPanelDialog(
    open: RwSignal<bool>,
    metric_names: RwSignal<Vec<String>>,
    on_add: Callback<Panel>,
) -> impl IntoView {
    let title = RwSignal::new(String::new());
    let metric = RwSignal::new(String::new());
    let chart_type = RwSignal::new("line".to_string());
    let range = RwSignal::new("1h".to_string());
    let agg = RwSignal::new("avg".to_string());
    let form_err: RwSignal<Option<String>> = RwSignal::new(None);

    let on_submit = move |_| {
        let t = title.get_untracked();
        let m = metric.get_untracked();
        if t.trim().is_empty() {
            form_err.set(Some("Title is required.".into()));
            return;
        }
        if m.trim().is_empty() {
            form_err.set(Some("Metric is required.".into()));
            return;
        }
        form_err.set(None);
        let panel = Panel {
            title: t,
            metric: m,
            chart_type: chart_type.get_untracked(),
            range: range.get_untracked(),
            agg: agg.get_untracked(),
        };
        on_add.run(panel);
        // Reset
        title.set(String::new());
        metric.set(String::new());
        open.set(false);
    };

    view! {
        <Dialog open=open>
            <DialogContent>
                <DialogHeader>
                    <DialogTitle>"Add Panel"</DialogTitle>
                </DialogHeader>
                <div class="space-y-4 py-2">
                    <div>
                        <label class="text-xs text-muted-foreground mb-1 block">"Title *"</label>
                        <Input value=title placeholder="e.g. CPU Usage".to_string() />
                    </div>
                    <div>
                        <label class="text-xs text-muted-foreground mb-1 block">"Metric *"</label>
                        {move || {
                            let names = metric_names.get();
                            if names.is_empty() {
                                view! { <Input value=metric placeholder="metric.name".to_string() /> }.into_any()
                            } else {
                                view! {
                                    <Select value=metric placeholder="Select metric…".to_string()>
                                        <SelectContent>
                                            <For
                                                each=move || metric_names.get()
                                                key=|n| n.clone()
                                                children=move |n| {
                                                    let v = n.clone();
                                                    view! { <SelectItem value=v.clone()>{v}</SelectItem> }
                                                }
                                            />
                                        </SelectContent>
                                    </Select>
                                }.into_any()
                            }
                        }}
                    </div>
                    <div class="grid grid-cols-3 gap-3">
                        <div>
                            <label class="text-xs text-muted-foreground mb-1 block">"Chart type"</label>
                            <Select value=chart_type placeholder="line".to_string()>
                                <SelectContent>
                                    <SelectItem value="line".to_string()>"Line"</SelectItem>
                                    <SelectItem value="area".to_string()>"Area"</SelectItem>
                                    <SelectItem value="bar".to_string()>"Bar"</SelectItem>
                                    <SelectItem value="stat".to_string()>"Stat"</SelectItem>
                                    <SelectItem value="sparkline".to_string()>"Sparkline"</SelectItem>
                                </SelectContent>
                            </Select>
                        </div>
                        <div>
                            <label class="text-xs text-muted-foreground mb-1 block">"Range"</label>
                            <Select value=range placeholder="1h".to_string()>
                                <SelectContent>
                                    <SelectItem value="15m".to_string()>"15m"</SelectItem>
                                    <SelectItem value="1h".to_string()>"1h"</SelectItem>
                                    <SelectItem value="6h".to_string()>"6h"</SelectItem>
                                    <SelectItem value="24h".to_string()>"24h"</SelectItem>
                                </SelectContent>
                            </Select>
                        </div>
                        <div>
                            <label class="text-xs text-muted-foreground mb-1 block">"Aggregation"</label>
                            <Select value=agg placeholder="avg".to_string()>
                                <SelectContent>
                                    <SelectItem value="avg".to_string()>"avg"</SelectItem>
                                    <SelectItem value="sum".to_string()>"sum"</SelectItem>
                                    <SelectItem value="min".to_string()>"min"</SelectItem>
                                    <SelectItem value="max".to_string()>"max"</SelectItem>
                                    <SelectItem value="count".to_string()>"count"</SelectItem>
                                </SelectContent>
                            </Select>
                        </div>
                    </div>
                    {move || form_err.get().map(|msg| view! {
                        <Alert variant=AlertVariant::Destructive>
                            <AlertDescription>{msg}</AlertDescription>
                        </Alert>
                    })}
                </div>
                <DialogFooter>
                    <div class="flex gap-2 justify-end">
                        <Button
                            variant=ButtonVariant::Outline
                            size=ButtonSize::Sm
                            on:click=move |_| open.set(false)
                        >
                            "Cancel"
                        </Button>
                        <Button
                            variant=ButtonVariant::Default
                            size=ButtonSize::Sm
                            on:click=on_submit
                        >
                            "Add"
                        </Button>
                    </div>
                </DialogFooter>
            </DialogContent>
        </Dialog>
    }
}

// ── Delete confirm dialog ─────────────────────────────────────────────────────

#[component]
fn DeleteDashboardDialog(
    open: RwSignal<bool>,
    dash_name: RwSignal<String>,
    on_confirm: Callback<()>,
) -> impl IntoView {
    view! {
        <Dialog open=open>
            <DialogContent>
                <DialogHeader>
                    <DialogTitle>"Delete dashboard?"</DialogTitle>
                </DialogHeader>
                <p class="text-sm text-muted-foreground py-2">
                    "Delete " {move || dash_name.get()} "? This cannot be undone."
                </p>
                <DialogFooter>
                    <div class="flex gap-2 justify-end">
                        <Button
                            variant=ButtonVariant::Outline
                            size=ButtonSize::Sm
                            on:click=move |_| open.set(false)
                        >
                            "Cancel"
                        </Button>
                        <Button
                            variant=ButtonVariant::Destructive
                            size=ButtonSize::Sm
                            on:click=move |_| on_confirm.run(())
                        >
                            "Delete"
                        </Button>
                    </div>
                </DialogFooter>
            </DialogContent>
        </Dialog>
    }
}

// ── Main page ─────────────────────────────────────────────────────────────────

#[component]
pub fn DashboardsPage() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("AppCtx required");

    // Dashboard list state
    let dash_list: RwSignal<Vec<DashboardSummary>> = RwSignal::new(vec![]);
    let list_loading = RwSignal::new(true);
    let list_err: RwSignal<Option<String>> = RwSignal::new(None);

    // Currently open dashboard
    let open_dash: RwSignal<Option<Dashboard>> = RwSignal::new(None);

    // Metric names for Add Panel dialog
    let metric_names: RwSignal<Vec<String>> = RwSignal::new(vec![]);

    // Dialog signals
    let add_panel_open = RwSignal::new(false);
    let delete_open = RwSignal::new(false);
    let delete_name = RwSignal::new(String::new());

    // Saving state
    let saving = RwSignal::new(false);
    let save_err: RwSignal<Option<String>> = RwSignal::new(None);

    // New dashboard name input (for inline "new dash" creation)
    let new_dash_name: RwSignal<String> = RwSignal::new(String::new());

    let token_sig = ctx.token;

    // Load dashboard list
    let load_list = move || {
        let token = token_sig.get_untracked();
        list_loading.set(true);
        list_err.set(None);
        leptos::task::spawn_local(async move {
            match list_dashboards(&token).await {
                Ok(v) => dash_list.set(v),
                Err(e) => list_err.set(Some(e.message)),
            }
            list_loading.set(false);
        });
    };

    // Load metric names for the add-panel form
    let load_names = move || {
        let token = token_sig.get_untracked();
        leptos::task::spawn_local(async move {
            if let Ok(names) = get_metric_names(&token).await {
                metric_names.set(names);
            }
        });
    };

    // Initial load
    Effect::new(move |_| {
        let _ = ctx.token.get();
        load_list();
        load_names();
    });

    // Open a dashboard by id
    let open_dash_by_id = move |id: i64| {
        let token = token_sig.get_untracked();
        leptos::task::spawn_local(async move {
            if let Ok(d) = crate::api::get_dashboard(&token, id).await {
                open_dash.set(Some(d));
            }
        });
    };

    // Create new dashboard
    let create_new = move |_| {
        let name = new_dash_name.get_untracked();
        if name.trim().is_empty() {
            return;
        }
        let token = token_sig.get_untracked();
        leptos::task::spawn_local(async move {
            if let Ok(d) = create_dashboard(&token, &name, &[]).await {
                let id = d.id;
                new_dash_name.set(String::new());
                load_list();
                open_dash_by_id(id);
            }
        });
    };

    // Add a panel to the open dashboard (in-memory only; user must save)
    let add_panel = Callback::new(move |panel: Panel| {
        open_dash.update(|od| {
            if let Some(d) = od.as_mut() {
                d.panels.push(panel);
            }
        });
    });

    // Remove a panel by index
    let remove_panel = move |idx: usize| {
        open_dash.update(|od| {
            if let Some(d) = od.as_mut() {
                if idx < d.panels.len() {
                    d.panels.remove(idx);
                }
            }
        });
    };

    // Save current dashboard
    let save_dash = move |_| {
        let Some(dash) = open_dash.get_untracked() else {
            return;
        };
        let token = token_sig.get_untracked();
        saving.set(true);
        save_err.set(None);
        leptos::task::spawn_local(async move {
            let result = if dash.id == 0 {
                create_dashboard(&token, &dash.name, &dash.panels)
                    .await
                    .map(|d| d)
            } else {
                update_dashboard(&token, dash.id, &dash.name, &dash.panels)
                    .await
                    .map(|d| d)
            };
            saving.set(false);
            match result {
                Ok(d) => {
                    open_dash.set(Some(d));
                    load_list();
                }
                Err(e) => save_err.set(Some(e.message)),
            }
        });
    };

    // Confirm delete
    let do_delete = Callback::new(move |_| {
        let Some(dash) = open_dash.get_untracked() else {
            return;
        };
        let token = token_sig.get_untracked();
        delete_open.set(false);
        leptos::task::spawn_local(async move {
            if delete_dashboard(&token, dash.id).await.is_ok() {
                open_dash.set(None);
                load_list();
            }
        });
    });

    view! {
        <div class="space-y-6">
            <PageHeader title="Dashboards".to_string()>
                <span class="text-xs text-muted-foreground">"Savable metric panel collections"</span>
            </PageHeader>

            // Dialogs
            <AddPanelDialog
                open=add_panel_open
                metric_names=metric_names
                on_add=add_panel
            />
            <DeleteDashboardDialog
                open=delete_open
                dash_name=delete_name
                on_confirm=do_delete
            />

            <div class="flex gap-6">
                // ── Left rail: dashboard list ─────────────────────────────────
                <aside class="w-56 shrink-0 space-y-3">
                    <Card>
                        <CardHeader>
                            <CardTitle>"Saved"</CardTitle>
                        </CardHeader>
                        <CardContent>
                            {move || {
                                if list_loading.get() {
                                    return view! { <div class="flex justify-center py-4"><Spinner /></div> }.into_any();
                                }
                                if let Some(msg) = list_err.get() {
                                    return view! {
                                        <Alert variant=AlertVariant::Destructive>
                                            <AlertDescription>{msg}</AlertDescription>
                                        </Alert>
                                    }.into_any();
                                }
                                let items = dash_list.get();
                                if items.is_empty() {
                                    return view! {
                                        <p class="text-xs text-muted-foreground">"No dashboards yet."</p>
                                    }.into_any();
                                }
                                view! {
                                    <ul class="space-y-1">
                                        <For
                                            each=move || dash_list.get()
                                            key=|d| d.id
                                            children=move |d: DashboardSummary| {
                                                let id = d.id;
                                                let name = d.name.clone();
                                                let is_active = Signal::derive(move || {
                                                    open_dash.get().map(|od| od.id == id).unwrap_or(false)
                                                });
                                                view! {
                                                    <li>
                                                        <button
                                                            class=move || if is_active.get() {
                                                                "w-full text-left text-sm px-2 py-1 rounded bg-accent text-accent-foreground"
                                                            } else {
                                                                "w-full text-left text-sm px-2 py-1 rounded hover:bg-accent/50 text-foreground"
                                                            }
                                                            on:click=move |_| open_dash_by_id(id)
                                                        >
                                                            {name}
                                                        </button>
                                                    </li>
                                                }
                                            }
                                        />
                                    </ul>
                                }.into_any()
                            }}
                        </CardContent>
                    </Card>

                    // New dashboard
                    <Card>
                        <CardHeader>
                            <CardTitle>"New dashboard"</CardTitle>
                        </CardHeader>
                        <CardContent>
                            <div class="space-y-2">
                                <Input value=new_dash_name placeholder="Dashboard name".to_string() />
                                <Button
                                    variant=ButtonVariant::Default
                                    size=ButtonSize::Sm
                                    on:click=create_new
                                >
                                    "Create"
                                </Button>
                            </div>
                        </CardContent>
                    </Card>
                </aside>

                // ── Main area: open dashboard ─────────────────────────────────
                <div class="flex-1 min-w-0">
                    {move || {
                        let Some(dash) = open_dash.get() else {
                            return view! {
                                <Empty
                                    title="No dashboard open".to_string()
                                    description="Select a saved dashboard or create a new one.".to_string()
                                />
                            }.into_any();
                        };

                        let dash_name = dash.name.clone();
                        let panels = dash.panels.clone();

                        view! {
                            <div class="space-y-4">
                                // Toolbar
                                <div class="flex items-center gap-3 flex-wrap">
                                    <span class="text-sm font-semibold text-foreground">{dash_name}</span>
                                    <Button
                                        variant=ButtonVariant::Outline
                                        size=ButtonSize::Sm
                                        on:click=move |_| add_panel_open.set(true)
                                    >
                                        "+ Add panel"
                                    </Button>
                                    <Button
                                        variant=ButtonVariant::Default
                                        size=ButtonSize::Sm
                                        disabled=saving.get()
                                        on:click=save_dash
                                    >
                                        {move || if saving.get() { "Saving…" } else { "Save" }}
                                    </Button>
                                    <Button
                                        variant=ButtonVariant::Destructive
                                        size=ButtonSize::Sm
                                        on:click=move |_| {
                                            if let Some(d) = open_dash.get_untracked() {
                                                delete_name.set(d.name.clone());
                                                delete_open.set(true);
                                            }
                                        }
                                    >
                                        "Delete dashboard"
                                    </Button>
                                </div>

                                // Save error
                                {move || save_err.get().map(|msg| view! {
                                    <Alert variant=AlertVariant::Destructive>
                                        <AlertTitle>"Save failed"</AlertTitle>
                                        <AlertDescription>{msg}</AlertDescription>
                                    </Alert>
                                })}

                                // Panel grid
                                {if panels.is_empty() {
                                    view! {
                                        <Empty
                                            title="No panels".to_string()
                                            description="Click \"+ Add panel\" to add a metric chart.".to_string()
                                        />
                                    }.into_any()
                                } else {
                                    // Extract indexed panels for the <For> key fn (avoids turbofish in view! attr).
                                    let panels_indexed = move || -> Vec<(usize, Panel)> {
                                        open_dash.get()
                                            .map(|d| d.panels.into_iter().enumerate().collect())
                                            .unwrap_or_default()
                                    };
                                    view! {
                                        <div class="grid grid-cols-2 gap-4">
                                            <For
                                                each=panels_indexed
                                                key=|(i, p)| format!("{}-{}", i, p.title)
                                                children=move |(idx, panel): (usize, Panel)| {
                                                    view! {
                                                        <PanelCard
                                                            panel=panel
                                                            on_remove=Callback::new(move |_| remove_panel(idx))
                                                        />
                                                    }
                                                }
                                            />
                                        </div>
                                    }.into_any()
                                }}
                            </div>
                        }.into_any()
                    }}
                </div>
            </div>
        </div>
    }
}
