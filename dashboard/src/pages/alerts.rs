//! Alerts page — manage alert rules and view active alerts.

use crate::api::{
    create_alert_rule, delete_alert_rule, list_active_alerts, list_alert_rules, update_alert_rule,
    ActiveAlert, AlertRule,
};
use crate::app::AppCtx;
use crate::util::relative_time;
use leptos::prelude::*;
use serde_json::json;
use soma_ui::{
    Alert, AlertDescription, AlertTitle, AlertVariant, Badge, BadgeVariant, Button, ButtonSize,
    ButtonVariant, Card, CardContent, CardHeader, CardTitle, Dialog, DialogContent, DialogFooter,
    DialogHeader, DialogTitle, Empty, Input, PageHeader, Select, SelectContent, SelectItem,
    Spinner, Switch, Table, TableBody, TableCell, TableHead, TableHeader, TableRow,
};

// ── Badge helpers ─────────────────────────────────────────────────────────────

fn severity_badge(s: &str) -> BadgeVariant {
    match s {
        "critical" => BadgeVariant::Destructive,
        "warning" => BadgeVariant::Default,
        _ => BadgeVariant::Secondary, // info
    }
}

fn state_badge(s: &str) -> BadgeVariant {
    match s {
        "firing" => BadgeVariant::Destructive,
        "pending" => BadgeVariant::Default,
        _ => BadgeVariant::Secondary, // ok / unknown
    }
}

// ── Config field helpers ──────────────────────────────────────────────────────

fn cfg_str<'a>(cfg: &'a serde_json::Value, key: &str) -> &'a str {
    cfg.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

fn cfg_f64(cfg: &serde_json::Value, key: &str) -> String {
    cfg.get(key)
        .and_then(|v| v.as_f64())
        .map(|f| f.to_string())
        .unwrap_or_default()
}

fn threshold_summary(rule: &AlertRule) -> String {
    let cfg = &rule.config;
    let cmp = cfg_str(cfg, "comparator");
    let thr = cfg_f64(cfg, "threshold");
    let win = cfg.get("window_secs").and_then(|v| v.as_i64()).unwrap_or(300);
    format!("{} {} ({}s)", cmp, thr, win)
}

fn webhook_host(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://") {
        rest.split('/').next().unwrap_or(rest).to_string()
    } else if let Some(rest) = url.strip_prefix("http://") {
        rest.split('/').next().unwrap_or(rest).to_string()
    } else {
        url.chars().take(30).collect()
    }
}

// ── Form state ────────────────────────────────────────────────────────────────

/// Holds the flat form state for create/edit.
/// All fields are RwSignal<T> which implement Copy, so this is also Copy.
#[derive(Clone, Copy)]
struct RuleForm {
    name: RwSignal<String>,
    kind: RwSignal<String>,
    severity: RwSignal<String>,
    webhook_url: RwSignal<String>,
    for_secs: RwSignal<String>,
    enabled: RwSignal<bool>,
    // metric fields
    metric_name: RwSignal<String>,
    agg: RwSignal<String>,
    comparator: RwSignal<String>,
    threshold: RwSignal<String>,
    window_secs: RwSignal<String>,
    filter: RwSignal<String>,
    // log fields
    log_filter: RwSignal<String>,
    severity_min: RwSignal<String>,
    q: RwSignal<String>,
    log_comparator: RwSignal<String>,
    log_threshold: RwSignal<String>,
    log_window_secs: RwSignal<String>,
}

impl RuleForm {
    fn new() -> Self {
        Self {
            name: RwSignal::new(String::new()),
            kind: RwSignal::new("metric".to_string()),
            severity: RwSignal::new("warning".to_string()),
            webhook_url: RwSignal::new(String::new()),
            for_secs: RwSignal::new("0".to_string()),
            enabled: RwSignal::new(true),
            metric_name: RwSignal::new(String::new()),
            agg: RwSignal::new("avg".to_string()),
            comparator: RwSignal::new("gt".to_string()),
            threshold: RwSignal::new(String::new()),
            window_secs: RwSignal::new("300".to_string()),
            filter: RwSignal::new(String::new()),
            log_filter: RwSignal::new(String::new()),
            severity_min: RwSignal::new(String::new()),
            q: RwSignal::new(String::new()),
            log_comparator: RwSignal::new("gt".to_string()),
            log_threshold: RwSignal::new(String::new()),
            log_window_secs: RwSignal::new("300".to_string()),
        }
    }

    /// Populate from an existing rule for editing.
    fn load(&self, rule: &AlertRule) {
        self.name.set(rule.name.clone());
        self.kind.set(rule.kind.clone());
        self.severity.set(rule.severity.clone());
        self.webhook_url
            .set(rule.webhook_url.clone().unwrap_or_default());
        self.for_secs.set(rule.for_secs.to_string());
        self.enabled.set(rule.enabled);

        let cfg = &rule.config;
        if rule.kind == "metric" {
            self.metric_name.set(cfg_str(cfg, "metric_name").to_string());
            self.agg.set(
                cfg_str(cfg, "agg")
                    .to_string()
                    .is_empty()
                    .then(|| "avg".to_string())
                    .unwrap_or_else(|| cfg_str(cfg, "agg").to_string()),
            );
            self.comparator.set(
                cfg_str(cfg, "comparator")
                    .to_string()
                    .is_empty()
                    .then(|| "gt".to_string())
                    .unwrap_or_else(|| cfg_str(cfg, "comparator").to_string()),
            );
            self.threshold.set(cfg_f64(cfg, "threshold"));
            self.window_secs.set(
                cfg.get("window_secs")
                    .and_then(|v| v.as_i64())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "300".to_string()),
            );
            self.filter.set(cfg_str(cfg, "filter").to_string());
        } else {
            self.log_filter.set(cfg_str(cfg, "filter").to_string());
            self.severity_min.set(
                cfg.get("severity_min")
                    .and_then(|v| v.as_i64())
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
            );
            self.q.set(cfg_str(cfg, "q").to_string());
            self.log_comparator.set(
                cfg_str(cfg, "comparator")
                    .to_string()
                    .is_empty()
                    .then(|| "gt".to_string())
                    .unwrap_or_else(|| cfg_str(cfg, "comparator").to_string()),
            );
            self.log_threshold.set(cfg_f64(cfg, "threshold"));
            self.log_window_secs.set(
                cfg.get("window_secs")
                    .and_then(|v| v.as_i64())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "300".to_string()),
            );
        }
    }

    /// Build the JSON body for POST (create).
    fn to_create_body(&self) -> serde_json::Value {
        let kind = self.kind.get_untracked();
        let config = self.build_config(&kind);
        let webhook = self.webhook_url.get_untracked();
        json!({
            "name": self.name.get_untracked(),
            "kind": kind,
            "enabled": self.enabled.get_untracked(),
            "severity": self.severity.get_untracked(),
            "config": config,
            "for_secs": self.for_secs.get_untracked().parse::<i32>().unwrap_or(0),
            "webhook_url": if webhook.is_empty() { serde_json::Value::Null } else { json!(webhook) },
        })
    }

    /// Build the JSON body for PUT (update) — sends full config, not a patch.
    fn to_update_body(&self) -> serde_json::Value {
        let kind = self.kind.get_untracked();
        let config = self.build_config(&kind);
        let webhook = self.webhook_url.get_untracked();
        // webhook_url uses Option<Option<String>> on server: Some(None) clears it.
        // We send Some(url) to set or Some(null) to clear. The server sees
        // `body.webhook_url.is_some() = true` and uses the inner value.
        json!({
            "name": self.name.get_untracked(),
            "enabled": self.enabled.get_untracked(),
            "severity": self.severity.get_untracked(),
            "config": config,
            "for_secs": self.for_secs.get_untracked().parse::<i32>().unwrap_or(0),
            "webhook_url": if webhook.is_empty() { serde_json::Value::Null } else { json!(webhook) },
        })
    }

    fn build_config(&self, kind: &str) -> serde_json::Value {
        if kind == "metric" {
            let mut cfg = json!({
                "metric_name": self.metric_name.get_untracked(),
                "agg": self.agg.get_untracked(),
                "comparator": self.comparator.get_untracked(),
                "threshold": self.threshold.get_untracked().parse::<f64>().unwrap_or(0.0),
                "window_secs": self.window_secs.get_untracked().parse::<i64>().unwrap_or(300),
            });
            let f = self.filter.get_untracked();
            if !f.is_empty() {
                cfg["filter"] = json!(f);
            }
            cfg
        } else {
            let mut cfg = json!({
                "comparator": self.log_comparator.get_untracked(),
                "threshold": self.log_threshold.get_untracked().parse::<f64>().unwrap_or(0.0),
                "window_secs": self.log_window_secs.get_untracked().parse::<i64>().unwrap_or(300),
            });
            let lf = self.log_filter.get_untracked();
            if !lf.is_empty() {
                cfg["filter"] = json!(lf);
            }
            let sm = self.severity_min.get_untracked();
            if let Ok(n) = sm.parse::<i64>() {
                cfg["severity_min"] = json!(n);
            }
            let q = self.q.get_untracked();
            if !q.is_empty() {
                cfg["q"] = json!(q);
            }
            cfg
        }
    }

    /// Client-side validation; returns an error message or None.
    fn validate(&self) -> Option<String> {
        if self.name.get_untracked().trim().is_empty() {
            return Some("Name is required.".to_string());
        }
        let kind = self.kind.get_untracked();
        if kind == "metric" {
            if self.metric_name.get_untracked().trim().is_empty() {
                return Some("Metric name is required.".to_string());
            }
            if self.threshold.get_untracked().parse::<f64>().is_err() {
                return Some("Threshold must be a number.".to_string());
            }
        } else {
            if self.log_threshold.get_untracked().parse::<f64>().is_err() {
                return Some("Threshold must be a number.".to_string());
            }
        }
        None
    }
}

// ── Rule form dialog ──────────────────────────────────────────────────────────

#[component]
fn RuleFormDialog(
    open: RwSignal<bool>,
    form: RuleForm,
    edit_id: RwSignal<Option<i64>>,
    on_saved: Callback<()>,
) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("AppCtx required");
    let submitting = RwSignal::new(false);
    let form_err: RwSignal<Option<String>> = RwSignal::new(None);

    let form2 = form.clone();
    let on_submit = move |_| {
        if let Some(msg) = form2.validate() {
            form_err.set(Some(msg));
            return;
        }
        form_err.set(None);
        submitting.set(true);

        let token = ctx.token.get_untracked();
        let id = edit_id.get_untracked();
        let body = if id.is_some() {
            form2.to_update_body()
        } else {
            form2.to_create_body()
        };

        leptos::task::spawn_local(async move {
            let result = if let Some(rule_id) = id {
                update_alert_rule(&token, rule_id, &body)
                    .await
                    .map(|_| ())
            } else {
                create_alert_rule(&token, &body).await.map(|_| ())
            };
            submitting.set(false);
            match result {
                Ok(()) => {
                    open.set(false);
                    on_saved.run(());
                }
                Err(e) => form_err.set(Some(e.message)),
            }
        });
    };

    let form_kind = form.kind;
    let title = Signal::derive(move || {
        if edit_id.get().is_some() {
            "Edit Rule".to_string()
        } else {
            "New Alert Rule".to_string()
        }
    });

    view! {
        <Dialog open=open>
            <DialogContent>
                <DialogHeader>
                    <DialogTitle>{move || title.get()}</DialogTitle>
                </DialogHeader>

                <div class="space-y-4 py-2 max-h-[60vh] overflow-y-auto pr-1">
                    // Name
                    <div>
                        <label class="text-xs text-muted-foreground mb-1 block">"Name *"</label>
                        <Input value=form.name placeholder="e.g. High CPU".to_string() />
                    </div>

                    // Kind + Severity row
                    <div class="grid grid-cols-2 gap-3">
                        <div>
                            <label class="text-xs text-muted-foreground mb-1 block">"Kind"</label>
                            <Select value=form.kind placeholder="Select kind".to_string()>
                                <SelectContent>
                                    <SelectItem value="metric".to_string()>"Metric"</SelectItem>
                                    <SelectItem value="log".to_string()>"Log"</SelectItem>
                                </SelectContent>
                            </Select>
                        </div>
                        <div>
                            <label class="text-xs text-muted-foreground mb-1 block">"Severity"</label>
                            <Select value=form.severity placeholder="Select severity".to_string()>
                                <SelectContent>
                                    <SelectItem value="info".to_string()>"Info"</SelectItem>
                                    <SelectItem value="warning".to_string()>"Warning"</SelectItem>
                                    <SelectItem value="critical".to_string()>"Critical"</SelectItem>
                                </SelectContent>
                            </Select>
                        </div>
                    </div>

                    // For secs + enabled row
                    <div class="grid grid-cols-2 gap-3 items-end">
                        <div>
                            <label class="text-xs text-muted-foreground mb-1 block">"For (seconds, 0 = immediate)"</label>
                            <Input value=form.for_secs placeholder="0".to_string() />
                        </div>
                        <div class="flex items-center gap-2 pb-1">
                            <Switch checked=form.enabled />
                            <span class="text-sm">"Enabled"</span>
                        </div>
                    </div>

                    // Webhook URL
                    <div>
                        <label class="text-xs text-muted-foreground mb-1 block">"Webhook URL (optional)"</label>
                        <Input value=form.webhook_url placeholder="https://hooks.example.com/…".to_string() />
                    </div>

                    // ── Conditional config ─────────────────────────────────────────────────

                    {move || {
                        let kind = form_kind.get();
                        if kind == "metric" {
                            view! {
                                <div class="space-y-3 border-t border-border pt-3 mt-3">
                                    <p class="text-xs font-medium text-muted-foreground">"Metric config"</p>
                                    <div>
                                        <label class="text-xs text-muted-foreground mb-1 block">"Metric name *"</label>
                                        <Input value=form.metric_name placeholder="cpu.usage".to_string() />
                                    </div>
                                    <div class="grid grid-cols-2 gap-3">
                                        <div>
                                            <label class="text-xs text-muted-foreground mb-1 block">"Aggregation"</label>
                                            <Select value=form.agg placeholder="avg".to_string()>
                                                <SelectContent>
                                                    <SelectItem value="avg".to_string()>"avg"</SelectItem>
                                                    <SelectItem value="sum".to_string()>"sum"</SelectItem>
                                                    <SelectItem value="min".to_string()>"min"</SelectItem>
                                                    <SelectItem value="max".to_string()>"max"</SelectItem>
                                                    <SelectItem value="count".to_string()>"count"</SelectItem>
                                                </SelectContent>
                                            </Select>
                                        </div>
                                        <div>
                                            <label class="text-xs text-muted-foreground mb-1 block">"Comparator"</label>
                                            <Select value=form.comparator placeholder="gt".to_string()>
                                                <SelectContent>
                                                    <SelectItem value="gt".to_string()>"gt (>)"</SelectItem>
                                                    <SelectItem value="gte".to_string()>"gte (>=)"</SelectItem>
                                                    <SelectItem value="lt".to_string()>"lt (<)"</SelectItem>
                                                    <SelectItem value="lte".to_string()>"lte (<=)"</SelectItem>
                                                </SelectContent>
                                            </Select>
                                        </div>
                                    </div>
                                    <div class="grid grid-cols-2 gap-3">
                                        <div>
                                            <label class="text-xs text-muted-foreground mb-1 block">"Threshold *"</label>
                                            <Input value=form.threshold placeholder="90".to_string() />
                                        </div>
                                        <div>
                                            <label class="text-xs text-muted-foreground mb-1 block">"Window (seconds)"</label>
                                            <Input value=form.window_secs placeholder="300".to_string() />
                                        </div>
                                    </div>
                                    <div>
                                        <label class="text-xs text-muted-foreground mb-1 block">"Filter (optional, key=\"value\")"</label>
                                        <Input value=form.filter placeholder="host=\"prod-1\"".to_string() />
                                    </div>
                                </div>
                            }.into_any()
                        } else {
                            view! {
                                <div class="space-y-3 border-t border-border pt-3 mt-3">
                                    <p class="text-xs font-medium text-muted-foreground">"Log config"</p>
                                    <div class="grid grid-cols-2 gap-3">
                                        <div>
                                            <label class="text-xs text-muted-foreground mb-1 block">"Comparator"</label>
                                            <Select value=form.log_comparator placeholder="gt".to_string()>
                                                <SelectContent>
                                                    <SelectItem value="gt".to_string()>"gt (>)"</SelectItem>
                                                    <SelectItem value="gte".to_string()>"gte (>=)"</SelectItem>
                                                </SelectContent>
                                            </Select>
                                        </div>
                                        <div>
                                            <label class="text-xs text-muted-foreground mb-1 block">"Threshold (count) *"</label>
                                            <Input value=form.log_threshold placeholder="10".to_string() />
                                        </div>
                                    </div>
                                    <div>
                                        <label class="text-xs text-muted-foreground mb-1 block">"Window (seconds)"</label>
                                        <Input value=form.log_window_secs placeholder="300".to_string() />
                                    </div>
                                    <div>
                                        <label class="text-xs text-muted-foreground mb-1 block">"Filter (optional, key=\"value\")"</label>
                                        <Input value=form.log_filter placeholder="service=\"api\"".to_string() />
                                    </div>
                                    <div>
                                        <label class="text-xs text-muted-foreground mb-1 block">"Min severity number (optional)"</label>
                                        <Input value=form.severity_min placeholder="17 (ERROR)".to_string() />
                                    </div>
                                    <div>
                                        <label class="text-xs text-muted-foreground mb-1 block">"Body search (optional)"</label>
                                        <Input value=form.q placeholder="panic".to_string() />
                                    </div>
                                </div>
                            }.into_any()
                        }
                    }}

                    // Form-level error
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
                            disabled=submitting.get()
                            on:click=on_submit
                        >
                            {move || if submitting.get() { "Saving…" } else { "Save" }}
                        </Button>
                    </div>
                </DialogFooter>
            </DialogContent>
        </Dialog>
    }
}

// ── Delete confirm dialog ─────────────────────────────────────────────────────

#[component]
fn DeleteDialog(
    open: RwSignal<bool>,
    rule_id: RwSignal<Option<i64>>,
    rule_name: RwSignal<String>,
    on_deleted: Callback<()>,
) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("AppCtx required");
    let deleting = RwSignal::new(false);
    let del_err: RwSignal<Option<String>> = RwSignal::new(None);

    let do_delete = move |_| {
        let Some(id) = rule_id.get_untracked() else {
            return;
        };
        del_err.set(None);
        deleting.set(true);
        let token = ctx.token.get_untracked();
        leptos::task::spawn_local(async move {
            match delete_alert_rule(&token, id).await {
                Ok(()) => {
                    deleting.set(false);
                    open.set(false);
                    on_deleted.run(());
                }
                Err(e) => {
                    deleting.set(false);
                    del_err.set(Some(e.message));
                }
            }
        });
    };

    view! {
        <Dialog open=open>
            <DialogContent>
                <DialogHeader>
                    <DialogTitle>"Delete rule?"</DialogTitle>
                </DialogHeader>
                <p class="text-sm text-muted-foreground py-2">
                    "Delete " {move || rule_name.get()} "? This cannot be undone."
                </p>
                {move || del_err.get().map(|msg| view! {
                    <Alert variant=AlertVariant::Destructive>
                        <AlertDescription>{msg}</AlertDescription>
                    </Alert>
                })}
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
                            disabled=deleting.get()
                            on:click=do_delete
                        >
                            {move || if deleting.get() { "Deleting…" } else { "Delete" }}
                        </Button>
                    </div>
                </DialogFooter>
            </DialogContent>
        </Dialog>
    }
}

// ── Active alerts banner ──────────────────────────────────────────────────────

#[component]
fn ActiveAlertsBanner(alerts: Vec<ActiveAlert>) -> impl IntoView {
    if alerts.is_empty() {
        return view! {
            <Alert variant=AlertVariant::Success>
                <AlertTitle>"No active alerts"</AlertTitle>
                <AlertDescription>"All rules are in OK state."</AlertDescription>
            </Alert>
        }
        .into_any();
    }

    let firing: Vec<_> = alerts.iter().filter(|a| a.state == "firing").cloned().collect();
    let pending: Vec<_> = alerts.iter().filter(|a| a.state == "pending").cloned().collect();

    view! {
        <div class="space-y-2">
            {firing.into_iter().map(|a| {
                let since = relative_time(&a.since);
                let msg = a.last_message.as_deref().unwrap_or("").to_string();
                let val = a.last_value.map(|v| format!(" (value: {:.4})", v)).unwrap_or_default();
                view! {
                    <Alert variant=AlertVariant::Destructive>
                        <AlertTitle>
                            {a.name.clone()}
                            " "
                            <Badge variant=BadgeVariant::Destructive>{a.severity.clone()}</Badge>
                        </AlertTitle>
                        <AlertDescription>
                            "Firing since "{since}{val}
                            {(!msg.is_empty()).then(|| view! { " — "{msg} })}
                        </AlertDescription>
                    </Alert>
                }
            }).collect::<Vec<_>>()}
            {pending.into_iter().map(|a| {
                let since = relative_time(&a.since);
                view! {
                    <Alert variant=AlertVariant::Warning>
                        <AlertTitle>
                            {a.name.clone()}
                            " "
                            <Badge variant=BadgeVariant::Default>{a.severity.clone()}</Badge>
                        </AlertTitle>
                        <AlertDescription>"Pending since "{since}</AlertDescription>
                    </Alert>
                }
            }).collect::<Vec<_>>()}
        </div>
    }
    .into_any()
}

// ── Main page ─────────────────────────────────────────────────────────────────

#[component]
pub fn AlertsPage() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("AppCtx required");

    let rules: RwSignal<Vec<AlertRule>> = RwSignal::new(vec![]);
    let active: RwSignal<Vec<ActiveAlert>> = RwSignal::new(vec![]);
    let loading = RwSignal::new(true);
    let err: RwSignal<Option<String>> = RwSignal::new(None);

    // Dialog state
    let dialog_open = RwSignal::new(false);
    let edit_id: RwSignal<Option<i64>> = RwSignal::new(None);
    let delete_open = RwSignal::new(false);
    let delete_id: RwSignal<Option<i64>> = RwSignal::new(None);
    let delete_name = RwSignal::new(String::new());

    let form = RuleForm::new();

    let token_sig = ctx.token;

    // Load both lists.
    let load = move || {
        let token = token_sig.get_untracked();
        loading.set(true);
        err.set(None);
        leptos::task::spawn_local(async move {
            match list_alert_rules(&token).await {
                Ok(r) => rules.set(r),
                Err(e) => err.set(Some(e.message)),
            }
            match list_active_alerts(&token).await {
                Ok(a) => active.set(a),
                Err(_) => {} // non-fatal; banner will just be empty
            }
            loading.set(false);
        });
    };

    // Initial load on token change.
    let load_clone = load.clone();
    Effect::new(move |_| {
        let _ = ctx.token.get(); // track token
        load_clone();
    });

    let on_saved = Callback::new(move |_| load());
    let on_deleted = {
        let load2 = {
            let token_sig2 = token_sig;
            move || {
                let token = token_sig2.get_untracked();
                loading.set(true);
                err.set(None);
                leptos::task::spawn_local(async move {
                    match list_alert_rules(&token).await {
                        Ok(r) => rules.set(r),
                        Err(e) => err.set(Some(e.message)),
                    }
                    match list_active_alerts(&token).await {
                        Ok(a) => active.set(a),
                        Err(_) => {}
                    }
                    loading.set(false);
                });
            }
        };
        Callback::new(move |_| load2())
    };

    let form_clone = form.clone();
    let open_new = move |_| {
        edit_id.set(None);
        // Reset form fields
        form_clone.name.set(String::new());
        form_clone.kind.set("metric".to_string());
        form_clone.severity.set("warning".to_string());
        form_clone.webhook_url.set(String::new());
        form_clone.for_secs.set("0".to_string());
        form_clone.enabled.set(true);
        form_clone.metric_name.set(String::new());
        form_clone.agg.set("avg".to_string());
        form_clone.comparator.set("gt".to_string());
        form_clone.threshold.set(String::new());
        form_clone.window_secs.set("300".to_string());
        form_clone.filter.set(String::new());
        form_clone.log_filter.set(String::new());
        form_clone.severity_min.set(String::new());
        form_clone.q.set(String::new());
        form_clone.log_comparator.set("gt".to_string());
        form_clone.log_threshold.set(String::new());
        form_clone.log_window_secs.set("300".to_string());
        dialog_open.set(true);
    };

    view! {
        <div class="space-y-6">
            <PageHeader title="Alerts".to_string()>
                <Button
                    variant=ButtonVariant::Default
                    size=ButtonSize::Sm
                    on:click=open_new
                >
                    "+ New Rule"
                </Button>
            </PageHeader>

            // Dialogs (always in tree, controlled by signals)
            <RuleFormDialog
                open=dialog_open
                form=form.clone()
                edit_id=edit_id
                on_saved=on_saved
            />
            <DeleteDialog
                open=delete_open
                rule_id=delete_id
                rule_name=delete_name
                on_deleted=on_deleted
            />

            // Error
            {move || err.get().map(|msg| view! {
                <Alert variant=AlertVariant::Destructive>
                    <AlertTitle>"Failed to load alerts"</AlertTitle>
                    <AlertDescription>{msg}</AlertDescription>
                </Alert>
            })}

            // Loading
            {move || loading.get().then(|| view! {
                <div class="flex justify-center py-8"><Spinner /></div>
            })}

            // Content (only when not loading)
            {move || {
                if loading.get() {
                    return ().into_any();
                }

                let active_snap = active.get();
                let rules_snap = rules.get();

                view! {
                    <div class="space-y-6">
                        // Active alerts banner
                        <Card>
                            <CardHeader>
                                <CardTitle>"Active Alerts"</CardTitle>
                            </CardHeader>
                            <CardContent>
                                <ActiveAlertsBanner alerts=active_snap />
                            </CardContent>
                        </Card>

                        // Rules table
                        <Card>
                            <CardHeader>
                                <CardTitle>"Rules"</CardTitle>
                            </CardHeader>
                            <CardContent>
                                {if rules_snap.is_empty() {
                                    view! {
                                        <Empty
                                            title="No alert rules".to_string()
                                            description="Create a rule to start alerting.".to_string()
                                        />
                                    }.into_any()
                                } else {
                                    let form_for_table = form;
                                    view! {
                                        <Table>
                                            <TableHeader>
                                                <TableRow>
                                                    <TableHead>"Name"</TableHead>
                                                    <TableHead class="w-20".to_string()>"Kind"</TableHead>
                                                    <TableHead class="w-24".to_string()>"Severity"</TableHead>
                                                    <TableHead class="w-24".to_string()>"State"</TableHead>
                                                    <TableHead>"Threshold"</TableHead>
                                                    <TableHead class="w-28".to_string()>"Webhook"</TableHead>
                                                    <TableHead class="w-20".to_string()>"Enabled"</TableHead>
                                                    <TableHead class="w-28".to_string()>" "</TableHead>
                                                </TableRow>
                                            </TableHeader>
                                            <TableBody>
                                                <For
                                                    each=move || rules.get()
                                                    key=|r| r.id
                                                    children={
                                                        let form_inner = form_for_table.clone();
                                                        move |rule: AlertRule| {
                                                            let rid = rule.id;
                                                            let rname = rule.name.clone();
                                                            let sev = rule.severity.clone();
                                                            let kind = rule.kind.clone();
                                                            let enabled = rule.enabled;
                                                            let state_str = rule
                                                                .state
                                                                .as_ref()
                                                                .map(|s| s.state.as_str())
                                                                .unwrap_or("—")
                                                                .to_string();
                                                            let threshold = threshold_summary(&rule);
                                                            let hook = rule
                                                                .webhook_url
                                                                .as_deref()
                                                                .map(webhook_host)
                                                                .unwrap_or_else(|| "—".to_string());

                                                            let form_edit = form_inner.clone();
                                                            let rule_for_edit = rule.clone();
                                                            let open_edit = move |_| {
                                                                form_edit.load(&rule_for_edit);
                                                                edit_id.set(Some(rid));
                                                                dialog_open.set(true);
                                                            };

                                                            let rname_del = rname.clone();
                                                            let open_delete = move |_| {
                                                                delete_id.set(Some(rid));
                                                                delete_name.set(rname_del.clone());
                                                                delete_open.set(true);
                                                            };

                                                            view! {
                                                                <TableRow>
                                                                    <TableCell class="font-medium".to_string()>{rname}</TableCell>
                                                                    <TableCell class="text-xs text-muted-foreground".to_string()>{kind}</TableCell>
                                                                    <TableCell>
                                                                        <Badge variant=severity_badge(&sev)>{sev}</Badge>
                                                                    </TableCell>
                                                                    <TableCell>
                                                                        <Badge variant=state_badge(&state_str)>{state_str}</Badge>
                                                                    </TableCell>
                                                                    <TableCell class="text-xs text-muted-foreground".to_string()>{threshold}</TableCell>
                                                                    <TableCell class="text-xs text-muted-foreground".to_string()>{hook}</TableCell>
                                                                    <TableCell class="text-xs".to_string()>
                                                                        {if enabled { "yes" } else { "no" }}
                                                                    </TableCell>
                                                                    <TableCell>
                                                                        <div class="flex gap-1">
                                                                            <Button
                                                                                variant=ButtonVariant::Ghost
                                                                                size=ButtonSize::Sm
                                                                                on:click=open_edit
                                                                            >
                                                                                "Edit"
                                                                            </Button>
                                                                            <Button
                                                                                variant=ButtonVariant::Ghost
                                                                                size=ButtonSize::Sm
                                                                                on:click=open_delete
                                                                            >
                                                                                "Delete"
                                                                            </Button>
                                                                        </div>
                                                                    </TableCell>
                                                                </TableRow>
                                                            }
                                                        }
                                                    }
                                                />
                                            </TableBody>
                                        </Table>
                                    }.into_any()
                                }}
                            </CardContent>
                        </Card>
                    </div>
                }.into_any()
            }}
        </div>
    }
}
