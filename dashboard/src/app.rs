//! App shell: router, sidebar, header with token input.

use crate::pages::{LogsPage, MetricsPage, OverviewPage, RetentionPage};
use leptos::prelude::*;
use leptos_router::{
    components::{FlatRoutes, Route, Router},
    hooks::use_location,
    path,
};
use soma_ui::{Input, Sidebar, SidebarItem, ThemeToggle, STYLES};

fn local_storage_get(key: &str) -> Option<String> {
    web_sys::window()
        .and_then(|w| w.local_storage().ok()?)
        .and_then(|s| s.get_item(key).ok()?)
}

fn local_storage_set(key: &str, value: &str) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok()?) {
        let _ = storage.set_item(key, value);
    }
}

// ── Shared context ─────────────────────────────────────────────────────────────

/// Signals threaded via context so pages can read token without prop drilling.
#[derive(Clone, Copy)]
pub struct AppCtx {
    pub token: RwSignal<String>,
}

fn sidebar_items() -> Vec<SidebarItem> {
    vec![
        SidebarItem {
            label: "Overview".to_string(),
            href: "/".to_string(),
            icon: Some(soma_ui::icons::icondata::LuLayoutDashboard),
        },
        SidebarItem {
            label: "Metrics".to_string(),
            href: "/metrics".to_string(),
            icon: Some(soma_ui::icons::icondata::LuActivity),
        },
        SidebarItem {
            label: "Logs".to_string(),
            href: "/logs".to_string(),
            icon: Some(soma_ui::icons::icondata::LuList),
        },
        SidebarItem {
            label: "Retention".to_string(),
            href: "/retention".to_string(),
            icon: Some(soma_ui::icons::icondata::LuSettings),
        },
    ]
}

#[component]
fn AppShell(children: Children) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("AppCtx must be provided");
    let location = use_location();
    let active_path = Signal::derive(move || location.pathname.get());

    // Health status dot: poll /health on mount
    let healthy = RwSignal::new(false);
    leptos::task::spawn_local(async move {
        healthy.set(crate::api::get_health().await);
    });

    let brand = view! {
        <span class="font-heading font-bold text-lg text-foreground tracking-tight">
            "soma-observe"
        </span>
    }
    .into_any();

    view! {
        <div class="flex h-screen bg-background overflow-hidden">
            <Sidebar
                items=sidebar_items()
                active_path=active_path
                brand=brand
            />
            <div class="flex flex-col flex-1 overflow-hidden">
                // Top bar
                <header class="flex items-center justify-between px-4 h-auto min-h-14 py-2 border-b border-border bg-card shrink-0 gap-4 flex-wrap">
                    <div class="flex items-center gap-2">
                        <span
                            class=move || if healthy.get() {
                                "h-2 w-2 rounded-full bg-green-500"
                            } else {
                                "h-2 w-2 rounded-full bg-red-400"
                            }
                            title=move || if healthy.get() { "Server reachable" } else { "Server unreachable" }
                        />
                        <span class="font-heading font-semibold text-foreground text-sm">"soma-observe"</span>
                    </div>
                    <div class="flex items-center gap-2 flex-wrap">
                        <div class="w-56">
                            <Input
                                input_type="password".to_string()
                                value=ctx.token
                                placeholder="admin token (optional)".to_string()
                                on:change=move |e| {
                                    let v = event_target_value(&e);
                                    local_storage_set("soma_observe_token", &v);
                                    ctx.token.set(v);
                                }
                            />
                        </div>
                        <ThemeToggle />
                    </div>
                </header>
                // Page content
                <main class="flex-1 overflow-auto p-6">
                    {children()}
                </main>
            </div>
        </div>
    }
}

#[component]
pub fn App() -> impl IntoView {
    let token = RwSignal::new(local_storage_get("soma_observe_token").unwrap_or_default());
    provide_context(AppCtx { token });

    view! {
        <style>{STYLES}</style>
        <Router>
            <AppShell>
                <FlatRoutes fallback=|| view! { <div class="text-muted-foreground">"Page not found"</div> }>
                    <Route path=path!("/") view=OverviewPage />
                    <Route path=path!("/metrics") view=MetricsPage />
                    <Route path=path!("/logs") view=LogsPage />
                    <Route path=path!("/retention") view=RetentionPage />
                </FlatRoutes>
            </AppShell>
        </Router>
    }
}
