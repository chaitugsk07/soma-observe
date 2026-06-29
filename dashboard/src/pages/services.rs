//! Services page — service map + RED metrics.
//!
//! ponytail: custom inline SVG for the dependency graph instead of SchemaDiagram
//! (SchemaDiagram is an ERD with column/FK rows — it does not fit a free-form
//! service node-edge graph). Inline style is used for all runtime-chosen colors
//! so Tailwind purge cannot strip them.

use crate::api::{get_service_map, ServiceEdge, ServiceStats};
use crate::app::AppCtx;
use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_query_map};
use leptos_router::NavigateOptions;
use soma_ui::{
    Alert, AlertDescription, AlertTitle, AlertVariant, Badge, BadgeVariant, Button, ButtonSize,
    ButtonVariant, Card, CardContent, CardHeader, CardTitle, Empty, Input, PageHeader, Spinner,
    Table, TableBody, TableCell, TableHead, TableHeader, TableRow,
};

// ── Health badge ──────────────────────────────────────────────────────────────

fn health_badge(error_rate: f64, p99_ms: f64) -> (BadgeVariant, &'static str) {
    if error_rate >= 0.1 {
        (BadgeVariant::Destructive, "unhealthy")
    } else if error_rate > 0.0 || p99_ms > 1000.0 {
        (BadgeVariant::Outline, "degraded")
    } else {
        (BadgeVariant::Success, "healthy")
    }
}

// ── Service graph SVG ─────────────────────────────────────────────────────────

/// Node fill color by health. Always inline style — runtime values.
fn node_fill(error_rate: f64, p99_ms: f64) -> &'static str {
    if error_rate >= 0.1 {
        "#ef4444" // red
    } else if error_rate > 0.0 || p99_ms > 1000.0 {
        "#f59e0b" // amber
    } else {
        "#10b981" // emerald
    }
}

/// Layout: arrange nodes in a circle (or row when few).
fn node_positions(count: usize, cx: f64, cy: f64, r: f64) -> Vec<(f64, f64)> {
    if count == 0 {
        return vec![];
    }
    if count == 1 {
        return vec![(cx, cy)];
    }
    use std::f64::consts::PI;
    (0..count)
        .map(|i| {
            let angle = 2.0 * PI * i as f64 / count as f64 - PI / 2.0;
            (cx + r * angle.cos(), cy + r * angle.sin())
        })
        .collect()
}

#[component]
fn ServiceGraph(
    services: Vec<ServiceStats>,
    edges: Vec<ServiceEdge>,
    on_node_click: Callback<String>,
) -> impl IntoView {
    if services.is_empty() {
        return view! { <div /> }.into_any();
    }

    // SVG canvas size
    let svg_w = 700.0_f64;
    let svg_h = 420.0_f64;
    let cx = svg_w / 2.0;
    let cy = svg_h / 2.0;
    // radius scales with service count, clamped
    let r = ((services.len() as f64 - 1.0) * 32.0 + 120.0).min(160.0).max(120.0);

    let node_r = 28.0_f64; // node circle radius

    // Build index map: service name -> position
    let positions: Vec<(String, f64, f64)> = {
        let pos = node_positions(services.len(), cx, cy, r);
        services
            .iter()
            .zip(pos.into_iter())
            .map(|(s, (x, y))| (s.name.clone(), x, y))
            .collect()
    };

    let pos_map: std::collections::HashMap<String, (f64, f64)> = positions
        .iter()
        .map(|(name, x, y)| (name.clone(), (*x, *y)))
        .collect();

    // Build edge SVG elements
    let edge_elems: Vec<_> = edges
        .iter()
        .filter_map(|e| {
            let (x1, y1) = pos_map.get(&e.from)?;
            let (x2, y2) = pos_map.get(&e.to)?;

            // Shorten the line so it doesn't go through the nodes
            let dx = x2 - x1;
            let dy = y2 - y1;
            let len = (dx * dx + dy * dy).sqrt().max(1.0);
            let ux = dx / len;
            let uy = dy / len;

            let sx = x1 + ux * node_r;
            let sy = y1 + uy * node_r;
            let ex = x2 - ux * (node_r + 6.0); // room for arrowhead
            let ey = y2 - uy * (node_r + 6.0);

            // Arrowhead
            let ax = ex + ux * 8.0;
            let ay = ey + uy * 8.0;
            let perp_x = -uy * 4.0;
            let perp_y = ux * 4.0;
            let arrow = format!(
                "M {},{} L {},{} L {},{}",
                ax,
                ay,
                ex + perp_x,
                ey + perp_y,
                ex - perp_x,
                ey - perp_y,
            );

            let stroke = if e.error_count > 0 { "#ef4444" } else { "#94a3b8" };
            let label = format!("{} calls", e.call_count);
            let mid_x = (sx + ex) / 2.0;
            let mid_y = (sy + ey) / 2.0 - 8.0;
            let p99_label = format!("p99 {:.0}ms", e.p99_ms);

            let stroke_owned = stroke.to_string();
            let stroke_owned2 = stroke.to_string();
            let stroke_owned3 = stroke.to_string();

            Some(view! {
                <g>
                    <line
                        x1=sx.to_string()
                        y1=sy.to_string()
                        x2=ex.to_string()
                        y2=ey.to_string()
                        stroke=stroke_owned
                        stroke-width="1.5"
                        stroke-opacity="0.8"
                    />
                    <path
                        d=arrow
                        fill=stroke_owned2
                        stroke="none"
                    />
                    <text
                        x=mid_x.to_string()
                        y=mid_y.to_string()
                        text-anchor="middle"
                        font-size="9"
                        font-family="monospace"
                        fill=stroke_owned3
                        opacity="0.85"
                    >
                        {label}
                    </text>
                    <text
                        x=mid_x.to_string()
                        y=(mid_y + 11.0).to_string()
                        text-anchor="middle"
                        font-size="9"
                        font-family="monospace"
                        fill="#94a3b8"
                        opacity="0.75"
                    >
                        {p99_label}
                    </text>
                </g>
            })
        })
        .collect();

    // Build node SVG elements
    let node_elems: Vec<_> = services
        .iter()
        .zip(positions.iter())
        .map(|(svc, (name, x, y))| {
            let fill = node_fill(svc.error_rate, svc.p99_ms);
            let svc_name = name.clone();
            let svc_name2 = svc_name.clone();
            let on_click = on_node_click.clone();
            // Truncate long names for display
            let display = if svc_name.len() > 14 {
                format!("{}…", &svc_name[..13])
            } else {
                svc_name.clone()
            };
            let rate_label = format!("{:.2}/s", svc.rate_per_sec);
            let x = *x;
            let y = *y;
            let fill_str = fill.to_string();
            let fill_str2 = fill.to_string();

            view! {
                <g
                    style="cursor:pointer;"
                    on:click=move |_| on_click.run(svc_name2.clone())
                >
                    <circle
                        cx=x.to_string()
                        cy=y.to_string()
                        r=node_r.to_string()
                        style=format!("fill:{};", fill_str)
                        stroke="white"
                        stroke-width="1.5"
                        opacity="0.92"
                    />
                    // Subtle dark overlay ring for depth
                    <circle
                        cx=x.to_string()
                        cy=y.to_string()
                        r=node_r.to_string()
                        fill="none"
                        stroke=fill_str2
                        stroke-width="3"
                        opacity="0.3"
                    />
                    <text
                        x=x.to_string()
                        y=(y - 4.0).to_string()
                        text-anchor="middle"
                        dominant-baseline="middle"
                        font-size="10"
                        font-weight="600"
                        font-family="sans-serif"
                        fill="white"
                    >
                        {display}
                    </text>
                    <text
                        x=x.to_string()
                        y=(y + 10.0).to_string()
                        text-anchor="middle"
                        dominant-baseline="middle"
                        font-size="8"
                        font-family="monospace"
                        fill="white"
                        opacity="0.85"
                    >
                        {rate_label}
                    </text>
                </g>
            }
        })
        .collect();

    view! {
        <svg
            width=svg_w.to_string()
            height=svg_h.to_string()
            style="max-width:100%; display:block;"
            viewBox=format!("0 0 {} {}", svg_w, svg_h)
        >
            // Dot grid background
            <defs>
                <pattern id="dotgrid" x="0" y="0" width="20" height="20" patternUnits="userSpaceOnUse">
                    <circle cx="1" cy="1" r="1" fill="#94a3b8" opacity="0.2" />
                </pattern>
            </defs>
            <rect width=svg_w.to_string() height=svg_h.to_string() fill="url(#dotgrid)" />
            {edge_elems}
            {node_elems}
        </svg>
    }
    .into_any()
}

// ── ServicesPage ──────────────────────────────────────────────────────────────

#[component]
pub fn ServicesPage() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("AppCtx required");
    let navigate = use_navigate();
    let query_map = use_query_map();

    let start_val = RwSignal::new(String::new());
    let end_val = RwSignal::new(String::new());

    let services: RwSignal<Vec<ServiceStats>> = RwSignal::new(vec![]);
    let edges: RwSignal<Vec<ServiceEdge>> = RwSignal::new(vec![]);
    let loading = RwSignal::new(false);
    let err: RwSignal<Option<String>> = RwSignal::new(None);
    let query_done = RwSignal::new(false);

    let token_sig = ctx.token;

    let run_query = move || {
        let token = token_sig.get_untracked();
        let start = start_val.get_untracked();
        let end = end_val.get_untracked();

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

        leptos::task::spawn_local(async move {
            match get_service_map(&token, Some(&s), if e.is_empty() { None } else { Some(&e) })
                .await
            {
                Ok(sm) => {
                    services.set(sm.services);
                    edges.set(sm.edges);
                }
                Err(e) => err.set(Some(e.message)),
            }
            loading.set(false);
            query_done.set(true);
        });
    };
    let do_query = {
        let run_query = run_query.clone();
        move |_: web_sys::MouseEvent| run_query()
    };

    // Auto-load on mount with defaults (last 1h)
    Effect::new(move |_| {
        let _ = query_map.get(); // track once
        run_query();
    });

    let nav_to_traces = {
        let navigate = navigate.clone();
        Callback::new(move |svc_name: String| {
            let href = format!("/traces?service={}", svc_name);
            navigate(&href, NavigateOptions::default());
        })
    };

    view! {
        <div class="space-y-6">
            <PageHeader title="Services".to_string()>
                <Button
                    variant=ButtonVariant::Default
                    size=ButtonSize::Sm
                    on:click=do_query
                >
                    "Refresh"
                </Button>
            </PageHeader>

            // Time range
            <div class="flex items-end gap-3 flex-wrap">
                <div class="w-44">
                    <label class="text-xs text-muted-foreground mb-1 block">"Start (unix or RFC3339)"</label>
                    <Input value=start_val placeholder="default: -1h".to_string() />
                </div>
                <div class="w-44">
                    <label class="text-xs text-muted-foreground mb-1 block">"End"</label>
                    <Input value=end_val placeholder="default: now".to_string() />
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

            // Main content
            {move || {
                if loading.get() {
                    return view! { <div class="flex justify-center py-8"><Spinner /></div> }.into_any();
                }
                if !query_done.get() {
                    return view! { <p class="text-sm text-muted-foreground">"Loading…"</p> }.into_any();
                }
                let svc_list = services.get();
                if svc_list.is_empty() {
                    return view! {
                        <Empty
                            title="No service data".to_string()
                            description="Send some traces with SERVER-kind spans to populate the service map.".to_string()
                        />
                    }.into_any();
                }

                let edge_list = edges.get();
                let nav_cb = nav_to_traces.clone();

                view! {
                    <div class="space-y-6">
                        // RED table
                        <Card>
                            <CardHeader>
                                <CardTitle>"RED Metrics"</CardTitle>
                            </CardHeader>
                            <CardContent>
                                <Table>
                                    <TableHeader>
                                        <TableRow>
                                            <TableHead>"Service"</TableHead>
                                            <TableHead class="w-24".to_string()>"Rate (r/s)"</TableHead>
                                            <TableHead class="w-24".to_string()>"Error %"</TableHead>
                                            <TableHead class="w-24".to_string()>"p50 ms"</TableHead>
                                            <TableHead class="w-24".to_string()>"p90 ms"</TableHead>
                                            <TableHead class="w-24".to_string()>"p99 ms"</TableHead>
                                            <TableHead class="w-24".to_string()>"Health"</TableHead>
                                        </TableRow>
                                    </TableHeader>
                                    <TableBody>
                                        <For
                                            each=move || services.get()
                                            key=|s| s.name.clone()
                                            children={
                                                let nav_cb = nav_to_traces.clone();
                                                move |svc| {
                                                    let svc_name = svc.name.clone();
                                                    let svc_name2 = svc_name.clone();
                                                    let (badge_variant, badge_label) = health_badge(svc.error_rate, svc.p99_ms);
                                                    let error_pct = format!("{:.1}%", svc.error_rate * 100.0);
                                                    let rate = format!("{:.2}", svc.rate_per_sec);
                                                    let p50 = format!("{:.1}", svc.p50_ms);
                                                    let p90 = format!("{:.1}", svc.p90_ms);
                                                    let p99 = format!("{:.1}", svc.p99_ms);
                                                    let nav = nav_cb.clone();
                                                    view! {
                                                        <TableRow class="cursor-pointer hover:bg-muted/40".to_string()>
                                                            <TableCell>
                                                                <button
                                                                    class="text-sm font-medium text-foreground hover:underline"
                                                                    on:click=move |_| nav.run(svc_name2.clone())
                                                                >
                                                                    {svc_name}
                                                                </button>
                                                            </TableCell>
                                                            <TableCell class="text-xs tabular-nums".to_string()>{rate}</TableCell>
                                                            <TableCell class="text-xs tabular-nums".to_string()>{error_pct}</TableCell>
                                                            <TableCell class="text-xs tabular-nums".to_string()>{p50}</TableCell>
                                                            <TableCell class="text-xs tabular-nums".to_string()>{p90}</TableCell>
                                                            <TableCell class="text-xs tabular-nums".to_string()>{p99}</TableCell>
                                                            <TableCell>
                                                                <Badge variant=badge_variant>{badge_label}</Badge>
                                                            </TableCell>
                                                        </TableRow>
                                                    }
                                                }
                                            }
                                        />
                                    </TableBody>
                                </Table>
                            </CardContent>
                        </Card>

                        // Dependency graph
                        <Card>
                            <CardHeader>
                                <CardTitle>"Dependency Graph"</CardTitle>
                            </CardHeader>
                            <CardContent>
                                <ServiceGraph
                                    services=svc_list
                                    edges=edge_list
                                    on_node_click=nav_cb
                                />
                            </CardContent>
                        </Card>
                    </div>
                }.into_any()
            }}
        </div>
    }
}
