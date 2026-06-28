//! Logs page — query and browse log records.

use crate::api::{query_logs, LogRecord};
use crate::app::AppCtx;
use crate::util::relative_time;
use leptos::prelude::*;
use soma_ui::{
    Alert, AlertDescription, AlertTitle, AlertVariant, Badge, BadgeVariant, Button, ButtonSize,
    ButtonVariant, Empty, Input, PageHeader, Select, SelectContent, SelectItem, Spinner, Table,
    TableBody, TableCell, TableHead, TableHeader, TableRow,
};

fn severity_variant(sev_num: Option<i32>) -> BadgeVariant {
    // OTEL severity numbers: 1-4 TRACE, 5-8 DEBUG, 9-12 INFO, 13-16 WARN, 17-20 ERROR, 21-24 FATAL
    match sev_num.unwrap_or(0) {
        1..=8 => BadgeVariant::Secondary,
        9..=12 => BadgeVariant::Default,
        13..=16 => BadgeVariant::Outline,
        17..=20 => BadgeVariant::Destructive,
        21..=24 => BadgeVariant::Destructive,
        _ => BadgeVariant::Secondary,
    }
}

fn service_from_resource(r: &serde_json::Value) -> String {
    r.as_object()
        .and_then(|o| o.get("service.name"))
        .and_then(|v| v.as_str())
        .unwrap_or("—")
        .to_string()
}

fn short_body(body: &str) -> String {
    let truncated: String = body.chars().take(120).collect();
    if body.len() > 120 {
        format!("{}…", truncated)
    } else {
        truncated
    }
}

#[component]
pub fn LogsPage() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("AppCtx required");

    let start_val = RwSignal::new(String::new());
    let end_val = RwSignal::new(String::new());
    let severity_min = RwSignal::new(String::new());
    let q_val = RwSignal::new(String::new());
    let filter_val = RwSignal::new(String::new());
    let limit_val = RwSignal::new("100".to_string());

    let logs: RwSignal<Vec<LogRecord>> = RwSignal::new(vec![]);
    let loading = RwSignal::new(false);
    let err: RwSignal<Option<String>> = RwSignal::new(None);
    let query_done = RwSignal::new(false);

    let token_sig = ctx.token;

    let do_query = move |_| {
        let token = token_sig.get_untracked();
        let start = start_val.get_untracked();
        let end = end_val.get_untracked();
        let sev = severity_min.get_untracked();
        let q = q_val.get_untracked();
        let filter = filter_val.get_untracked();
        let limit_s = limit_val.get_untracked();
        let limit = limit_s.parse::<u32>().unwrap_or(100);

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
            match query_logs(
                &token,
                Some(&s),
                if e.is_empty() { None } else { Some(&e) },
                if filter.is_empty() { None } else { Some(&filter) },
                if sev.is_empty() { None } else { Some(&sev) },
                if q.is_empty() { None } else { Some(&q) },
                Some(limit),
            )
            .await
            {
                Ok(records) => logs.set(records),
                Err(e) => err.set(Some(e.message)),
            }
            loading.set(false);
            query_done.set(true);
        });
    };

    // Refresh: same as do_query but reads current values
    let do_refresh = do_query;

    view! {
        <div class="space-y-6">
            <PageHeader title="Logs".to_string()>
                <Button
                    variant=ButtonVariant::Default
                    size=ButtonSize::Sm
                    on:click=do_query
                >
                    "Query"
                </Button>
            </PageHeader>

            // Filters
            <div class="flex items-end gap-3 flex-wrap">
                <div class="w-40">
                    <label class="text-xs text-muted-foreground mb-1 block">"Start (unix or RFC3339)"</label>
                    <Input value=start_val placeholder="default: -1h".to_string() />
                </div>
                <div class="w-40">
                    <label class="text-xs text-muted-foreground mb-1 block">"End"</label>
                    <Input value=end_val placeholder="default: now".to_string() />
                </div>
                <div class="w-36">
                    <label class="text-xs text-muted-foreground mb-1 block">"Min severity"</label>
                    <Select value=severity_min placeholder="Any severity".to_string()>
                        <SelectContent>
                            <SelectItem value="".to_string()>"Any"</SelectItem>
                            <SelectItem value="5".to_string()>"DEBUG (5)"</SelectItem>
                            <SelectItem value="9".to_string()>"INFO (9)"</SelectItem>
                            <SelectItem value="13".to_string()>"WARN (13)"</SelectItem>
                            <SelectItem value="17".to_string()>"ERROR (17)"</SelectItem>
                            <SelectItem value="21".to_string()>"FATAL (21)"</SelectItem>
                        </SelectContent>
                    </Select>
                </div>
                <div class="w-48">
                    <label class="text-xs text-muted-foreground mb-1 block">"Body search"</label>
                    <Input value=q_val placeholder="search text".to_string() />
                </div>
                <div class="w-48">
                    <label class="text-xs text-muted-foreground mb-1 block">"Attribute filter"</label>
                    <Input value=filter_val placeholder="key=value".to_string() />
                </div>
                <div class="w-24">
                    <label class="text-xs text-muted-foreground mb-1 block">"Limit"</label>
                    <Input value=limit_val placeholder="100".to_string() />
                </div>
                <Button
                    variant=ButtonVariant::Outline
                    size=ButtonSize::Sm
                    on:click=do_query
                >
                    "Apply"
                </Button>
            </div>

            // Error
            {move || err.get().map(|msg| view! {
                <Alert variant=AlertVariant::Destructive>
                    <AlertTitle>"Query failed"</AlertTitle>
                    <AlertDescription>{msg}</AlertDescription>
                </Alert>
            })}

            // Table
            {move || {
                if loading.get() {
                    return view! { <div class="flex justify-center py-8"><Spinner /></div> }.into_any();
                }
                if !query_done.get() {
                    return view! {
                        <p class="text-sm text-muted-foreground">"Set filters and click Query to load logs."</p>
                    }.into_any();
                }
                let records = logs.get();
                if records.is_empty() {
                    return view! {
                        <Empty
                            title="No log records".to_string()
                            description="No records matched the current filters.".to_string()
                        />
                    }.into_any();
                }
                let count = records.len();
                view! {
                    <div class="space-y-3">
                        <p class="text-xs text-muted-foreground">{format!("{} records", count)}</p>
                        <Table>
                            <TableHeader>
                                <TableRow>
                                    <TableHead class="w-36".to_string()>"Time"</TableHead>
                                    <TableHead class="w-24".to_string()>"Severity"</TableHead>
                                    <TableHead>"Body"</TableHead>
                                    <TableHead class="w-32".to_string()>"Service"</TableHead>
                                </TableRow>
                            </TableHeader>
                            <TableBody>
                                <For
                                    each=move || logs.get()
                                    key=|r| r.id
                                    children=move |rec| {
                                        let ts = rec.ts.clone();
                                        let rel = relative_time(&ts);
                                        let sev_num = rec.severity_number;
                                        let sev_text = match rec.severity_text.as_deref() {
                                            Some(t) if !t.is_empty() => t.to_string(),
                                            _ => sev_num.map(|n| n.to_string()).unwrap_or_else(|| "—".to_string()),
                                        };
                                        let body_str = rec.body.as_deref().unwrap_or("");
                                        let body = short_body(body_str);
                                        let full_body = rec.body.clone().unwrap_or_default();
                                        let service = service_from_resource(&rec.resource);
                                        view! {
                                            <TableRow>
                                                <TableCell class="text-xs text-muted-foreground whitespace-nowrap".to_string()>
                                                    <span title=ts>{rel}</span>
                                                </TableCell>
                                                <TableCell>
                                                    <Badge variant=severity_variant(sev_num)>
                                                        {sev_text}
                                                    </Badge>
                                                </TableCell>
                                                <TableCell class="text-xs max-w-xs".to_string()>
                                                    <span title=full_body>{body}</span>
                                                </TableCell>
                                                <TableCell class="text-xs text-muted-foreground".to_string()>
                                                    {service}
                                                </TableCell>
                                            </TableRow>
                                        }
                                    }
                                />
                            </TableBody>
                        </Table>
                        <div class="flex justify-center pt-2">
                            <Button
                                variant=ButtonVariant::Outline
                                size=ButtonSize::Sm
                                on:click=do_refresh
                            >
                                "Refresh"
                            </Button>
                        </div>
                    </div>
                }.into_any()
            }}
        </div>
    }
}
