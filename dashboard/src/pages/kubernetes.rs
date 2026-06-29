//! Kubernetes topology page — namespace → workload RED health.
//!
//! ponytail: no shared health_badge module — 6-line helper duplicated from
//! services.rs is cheaper than a shared module for two call sites.

use crate::api::{get_kubernetes_topology, K8sNamespace, K8sWorkload};
use crate::app::AppCtx;
use leptos::prelude::*;
use leptos_router::hooks::use_navigate;
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

// ── Pods cell helper ──────────────────────────────────────────────────────────

/// Show pod_count; if pod names are known, list up to 3 then "+N more".
fn pods_display(pod_count: i64, pods: &[String]) -> String {
    if pods.is_empty() {
        return pod_count.to_string();
    }
    let displayed: Vec<&str> = pods.iter().take(3).map(|s| s.as_str()).collect();
    let mut out = displayed.join(", ");
    let remaining = pods.len().saturating_sub(3);
    if remaining > 0 {
        out.push_str(&format!(" +{remaining} more"));
    }
    out
}

// ── Namespace card ────────────────────────────────────────────────────────────

/// ponytail: nav_to_traces is a Callback<String> (workload name → trace URL)
/// instead of threading the opaque `impl Fn` navigate type through props.
#[component]
fn NamespaceCard(ns: K8sNamespace, nav_to_traces: Callback<String>) -> impl IntoView {
    let wl_count = ns.workloads.len();
    let ns_name = ns.name.clone();
    let workloads = ns.workloads;

    view! {
        <Card>
            <CardHeader>
                <div class="flex items-center gap-3 flex-wrap">
                    <CardTitle>{ns_name}</CardTitle>
                    <Badge variant=BadgeVariant::Secondary>
                        {format!("{wl_count} workload{}", if wl_count == 1 { "" } else { "s" })}
                    </Badge>
                    <Badge variant=BadgeVariant::Secondary>
                        {format!("{} pods", ns.pod_count)}
                    </Badge>
                    {(ns.error_count > 0).then(|| view! {
                        <Badge variant=BadgeVariant::Destructive>
                            {format!("{} errors", ns.error_count)}
                        </Badge>
                    })}
                </div>
            </CardHeader>
            <CardContent>
                <Table>
                    <TableHeader>
                        <TableRow>
                            <TableHead>"Workload"</TableHead>
                            <TableHead class="w-28".to_string()>"Kind"</TableHead>
                            <TableHead>"Pods"</TableHead>
                            <TableHead class="w-24".to_string()>"Rate (sp/s)"</TableHead>
                            <TableHead class="w-24".to_string()>"Error %"</TableHead>
                            <TableHead class="w-24".to_string()>"p50 ms"</TableHead>
                            <TableHead class="w-24".to_string()>"p99 ms"</TableHead>
                            <TableHead class="w-24".to_string()>"Health"</TableHead>
                        </TableRow>
                    </TableHeader>
                    <TableBody>
                        <For
                            each=move || workloads.clone()
                            key=|w| w.workload.clone()
                            children={
                                let nav_to_traces = nav_to_traces.clone();
                                move |wl: K8sWorkload| {
                                    let wl_name = wl.workload.clone();
                                    let wl_name_nav = wl_name.clone();
                                    let (badge_variant, badge_label) = health_badge(wl.error_rate, wl.p99_ms);
                                    let pods_text = pods_display(wl.pod_count, &wl.pods);
                                    let rate = format!("{:.2}", wl.rate_per_sec);
                                    let error_pct = format!("{:.1}%", wl.error_rate * 100.0);
                                    let p50 = format!("{:.1}", wl.p50_ms);
                                    let p99 = format!("{:.1}", wl.p99_ms);
                                    let nav = nav_to_traces.clone();

                                    view! {
                                        <TableRow class="cursor-pointer hover:bg-muted/40".to_string()>
                                            <TableCell>
                                                // ponytail: pivot to /traces?service=<workload> — matches only
                                                // when workload name equals service_name. Imperfect but useful.
                                                <button
                                                    class="text-sm font-medium text-foreground hover:underline"
                                                    on:click=move |_| nav.run(wl_name_nav.clone())
                                                >
                                                    {wl_name}
                                                </button>
                                            </TableCell>
                                            <TableCell class="text-xs text-muted-foreground".to_string()>
                                                {wl.kind}
                                            </TableCell>
                                            <TableCell class="text-xs tabular-nums".to_string()>
                                                {pods_text}
                                            </TableCell>
                                            <TableCell class="text-xs tabular-nums".to_string()>{rate}</TableCell>
                                            <TableCell class="text-xs tabular-nums".to_string()>{error_pct}</TableCell>
                                            <TableCell class="text-xs tabular-nums".to_string()>{p50}</TableCell>
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
    }
}

// ── KubernetesPage ────────────────────────────────────────────────────────────

#[component]
pub fn KubernetesPage() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("AppCtx required");
    let navigate = use_navigate();

    let nav_to_traces = {
        let navigate = navigate.clone();
        Callback::new(move |wl_name: String| {
            let href = format!("/traces?service={}", wl_name);
            navigate(&href, NavigateOptions::default());
        })
    };

    let start_val = RwSignal::new(String::new());
    let end_val = RwSignal::new(String::new());

    let topology: RwSignal<Option<crate::api::K8sTopology>> = RwSignal::new(None);
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
            match get_kubernetes_topology(
                &token,
                Some(&s),
                if e.is_empty() { None } else { Some(&e) },
            )
            .await
            {
                Ok(t) => topology.set(Some(t)),
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
        run_query();
    });

    view! {
        <div class="space-y-6">
            <PageHeader title="Kubernetes".to_string()>
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

                let Some(topo) = topology.get() else {
                    return view! { <p class="text-sm text-muted-foreground">"No data"</p> }.into_any();
                };

                if topo.namespaces.is_empty() {
                    return view! {
                        <Empty
                            title="No Kubernetes metadata".to_string()
                            description="No spans carry k8s.* resource attributes. Add the OpenTelemetry \
                                `k8sattributes` processor to your Collector so telemetry is tagged with \
                                namespace / pod / deployment. See docs/kubernetes.md.".to_string()
                        />
                    }.into_any();
                }

                let ns_count = topo.namespaces.len();
                let wl_count: usize = topo.namespaces.iter().map(|n| n.workloads.len()).sum();
                let node_count = topo.node_count;
                let summary = format!(
                    "{ns_count} namespace{} · {wl_count} workload{} · {node_count} node{}",
                    if ns_count == 1 { "" } else { "s" },
                    if wl_count == 1 { "" } else { "s" },
                    if node_count == 1 { "" } else { "s" },
                );

                let nav_cb = nav_to_traces.clone();
                view! {
                    <div class="space-y-6">
                        <p class="text-sm text-muted-foreground">{summary}</p>
                        <For
                            each=move || topo.namespaces.clone()
                            key=|ns| ns.name.clone()
                            children={
                                let nav_cb = nav_cb.clone();
                                move |ns: K8sNamespace| {
                                    view! { <NamespaceCard ns=ns nav_to_traces=nav_cb.clone() /> }
                                }
                            }
                        />
                    </div>
                }.into_any()
            }}
        </div>
    }
}
