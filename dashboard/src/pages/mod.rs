mod alerts;
pub mod kubernetes;
mod logs;
mod metrics;
mod overview;
mod retention;
pub mod services;
pub mod traces;

pub use alerts::AlertsPage;
pub use kubernetes::KubernetesPage;
pub use logs::LogsPage;
pub use metrics::MetricsPage;
pub use overview::OverviewPage;
pub use retention::RetentionPage;
pub use services::ServicesPage;
pub use traces::TracesPage;
