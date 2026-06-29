//! Overview page — system stats at a glance.

use crate::api::{get_health_full, get_stats, StatsResponse};
use crate::app::AppCtx;
use crate::util::fmt_bytes;
use leptos::prelude::*;
use soma_ui::{
    Alert, AlertDescription, AlertTitle, AlertVariant, Callout, CalloutVariant, Card, CardContent,
    CardHeader, CardTitle, PageHeader, Spinner, Stat, Status, StatusKind,
};

#[component]
pub fn OverviewPage() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("AppCtx required");
    let stats: RwSignal<Option<StatsResponse>> = RwSignal::new(None);
    let db_status = RwSignal::new(String::new());
    let err: RwSignal<Option<String>> = RwSignal::new(None);
    let loading = RwSignal::new(true);

    Effect::new(move |_| {
        let token = ctx.token.get();
        loading.set(true);
        err.set(None);
        leptos::task::spawn_local(async move {
            if let Ok(h) = get_health_full(&token).await {
                db_status.set(h.db);
            }
            match get_stats(&token).await {
                Ok(s) => stats.set(Some(s)),
                Err(e) => err.set(Some(e.message)),
            }
            loading.set(false);
        });
    });

    view! {
        <div class="space-y-6">
            <PageHeader title="Overview".to_string()>
                <span class="text-xs text-muted-foreground">"System stats"</span>
            </PageHeader>

            {move || err.get().map(|msg| view! {
                <Alert variant=AlertVariant::Destructive>
                    <AlertTitle>"Failed to load stats"</AlertTitle>
                    <AlertDescription>{msg}</AlertDescription>
                </Alert>
            })}

            {move || {
                if loading.get() {
                    return view! { <div class="flex justify-center py-8"><Spinner /></div> }.into_any();
                }
                let Some(s) = stats.get() else {
                    return ().into_any();
                };

                let db_ok = db_status.get() == "ok";

                view! {
                    <div class="space-y-6">
                        // Unauthenticated warning
                        {(!s.auth_required).then(|| view! {
                            <Callout variant=CalloutVariant::Warning title="Unauthenticated".to_string()>
                                "Ingest & query endpoints are unauthenticated. \
                                Set an AUTH_TOKEN environment variable to enable auth."
                            </Callout>
                        })}

                        // Stat cards
                        <div class="grid grid-cols-2 md:grid-cols-5 gap-4">
                            <Stat
                                label="Metric Series"
                                value=s.counts.series.to_string()
                            />
                            <Stat
                                label="Metric Points"
                                value=s.counts.metric_points.to_string()
                            />
                            <Stat
                                label="Histogram Points"
                                value=s.counts.histogram_points.to_string()
                            />
                            <Stat
                                label="Log Records"
                                value=s.counts.logs.to_string()
                            />
                            <Stat
                                label="Trace Spans"
                                value=s.counts.spans.to_string()
                            />
                        </div>

                        // Detail cards
                        <div class="grid grid-cols-1 md:grid-cols-3 gap-4">
                            <Card>
                                <CardHeader>
                                    <CardTitle>"Retention"</CardTitle>
                                </CardHeader>
                                <CardContent>
                                    <dl class="space-y-1 text-sm">
                                        <div class="flex justify-between">
                                            <dt class="text-muted-foreground">"Metrics"</dt>
                                            <dd>{s.retention.metrics_days}" days"</dd>
                                        </div>
                                        <div class="flex justify-between">
                                            <dt class="text-muted-foreground">"Logs"</dt>
                                            <dd>{s.retention.logs_days}" days"</dd>
                                        </div>
                                    </dl>
                                </CardContent>
                            </Card>

                            <Card>
                                <CardHeader>
                                    <CardTitle>"Database"</CardTitle>
                                </CardHeader>
                                <CardContent>
                                    <dl class="space-y-2 text-sm">
                                        <div class="flex justify-between items-center">
                                            <dt class="text-muted-foreground">"Health"</dt>
                                            <dd>
                                                <Status
                                                    kind=if db_ok { StatusKind::Online } else { StatusKind::Offline }
                                                    label=db_status.get()
                                                />
                                            </dd>
                                        </div>
                                        <div class="flex justify-between">
                                            <dt class="text-muted-foreground">"Size"</dt>
                                            <dd>{s.db_size_bytes.map(fmt_bytes).unwrap_or_else(|| "—".to_string())}</dd>
                                        </div>
                                        <div class="flex justify-between">
                                            <dt class="text-muted-foreground">"Partitions"</dt>
                                            <dd>{s.partitions.to_string()}</dd>
                                        </div>
                                    </dl>
                                </CardContent>
                            </Card>

                            <Card>
                                <CardHeader>
                                    <CardTitle>"Security"</CardTitle>
                                </CardHeader>
                                <CardContent>
                                    <dl class="space-y-1 text-sm">
                                        <div class="flex justify-between">
                                            <dt class="text-muted-foreground">"Auth required"</dt>
                                            <dd>{if s.auth_required { "yes" } else { "no" }}</dd>
                                        </div>
                                    </dl>
                                </CardContent>
                            </Card>
                        </div>
                    </div>
                }.into_any()
            }}
        </div>
    }
}
