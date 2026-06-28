//! Retention page — read-only admin view from /api/v1/admin/stats.

use crate::api::{get_stats, StatsResponse};
use crate::app::AppCtx;
use crate::util::fmt_bytes;
use leptos::prelude::*;
use soma_ui::{
    Alert, AlertDescription, AlertTitle, AlertVariant, Callout, CalloutVariant, Card, CardContent,
    CardHeader, CardTitle, Column, DataTable, PageHeader, Spinner, Stat,
};
use std::collections::HashMap;

fn stats_to_rows(s: &StatsResponse) -> Vec<HashMap<String, String>> {
    vec![
        {
            let mut m = HashMap::new();
            m.insert("key".to_string(), "Metrics retention".to_string());
            m.insert("value".to_string(), format!("{} days", s.retention.metrics_days));
            m
        },
        {
            let mut m = HashMap::new();
            m.insert("key".to_string(), "Logs retention".to_string());
            m.insert("value".to_string(), format!("{} days", s.retention.logs_days));
            m
        },
        {
            let mut m = HashMap::new();
            m.insert("key".to_string(), "Active partitions".to_string());
            m.insert("value".to_string(), s.partitions.to_string());
            m
        },
        {
            let mut m = HashMap::new();
            m.insert("key".to_string(), "Metric series".to_string());
            m.insert("value".to_string(), s.counts.series.to_string());
            m
        },
        {
            let mut m = HashMap::new();
            m.insert("key".to_string(), "Metric points".to_string());
            m.insert("value".to_string(), s.counts.metric_points.to_string());
            m
        },
        {
            let mut m = HashMap::new();
            m.insert("key".to_string(), "Histogram points".to_string());
            m.insert("value".to_string(), s.counts.histogram_points.to_string());
            m
        },
        {
            let mut m = HashMap::new();
            m.insert("key".to_string(), "Log records".to_string());
            m.insert("value".to_string(), s.counts.logs.to_string());
            m
        },
        {
            let mut m = HashMap::new();
            m.insert("key".to_string(), "DB size".to_string());
            m.insert(
                "value".to_string(),
                s.db_size_bytes.map(fmt_bytes).unwrap_or_else(|| "—".to_string()),
            );
            m
        },
    ]
}

#[component]
pub fn RetentionPage() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("AppCtx required");
    let stats: RwSignal<Option<StatsResponse>> = RwSignal::new(None);
    let err: RwSignal<Option<String>> = RwSignal::new(None);
    let loading = RwSignal::new(true);

    Effect::new(move |_| {
        let token = ctx.token.get();
        loading.set(true);
        err.set(None);
        leptos::task::spawn_local(async move {
            match get_stats(&token).await {
                Ok(s) => stats.set(Some(s)),
                Err(e) => err.set(Some(e.message)),
            }
            loading.set(false);
        });
    });

    view! {
        <div class="space-y-6">
            <PageHeader title="Retention".to_string()>
                <span class="text-xs text-muted-foreground">"Data retention and storage admin view"</span>
            </PageHeader>

            <Callout variant=CalloutVariant::Info title="How retention works".to_string()>
                "Data is stored in time-based partitions. Expired partitions are DROPped in full \
                (fast, near-zero locking). The retention window shown below is the configured \
                minimum; actual deletion depends on the maintenance schedule."
            </Callout>

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

                let cols = vec![
                    Column {
                        key: "key".to_string(),
                        header: "Setting".to_string(),
                        sortable: false,
                        editable: false,
                    },
                    Column {
                        key: "value".to_string(),
                        header: "Value".to_string(),
                        sortable: false,
                        editable: false,
                    },
                ];
                let rows = stats_to_rows(&s);

                view! {
                    <div class="space-y-6">
                        // Stat cards
                        <div class="grid grid-cols-2 md:grid-cols-4 gap-4">
                            <Stat
                                label="Metrics retention"
                                value=format!("{} days", s.retention.metrics_days)
                            />
                            <Stat
                                label="Logs retention"
                                value=format!("{} days", s.retention.logs_days)
                            />
                            <Stat
                                label="Partitions"
                                value=s.partitions.to_string()
                            />
                            <Stat
                                label="DB size"
                                value=s.db_size_bytes.map(fmt_bytes).unwrap_or_else(|| "—".to_string())
                            />
                        </div>

                        // Detail table
                        <Card>
                            <CardHeader>
                                <CardTitle>"Storage Details"</CardTitle>
                            </CardHeader>
                            <CardContent>
                                <DataTable columns=cols rows=rows />
                            </CardContent>
                        </Card>
                    </div>
                }.into_any()
            }}
        </div>
    }
}
