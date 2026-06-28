//! Embedded SPA assets for the admin portal.
//!
//! Mirrors the pattern from soma-audit: RustEmbed over `dashboard/dist/` so
//! Trunk-built files are compiled in. The fallback route in `main.rs` delegates
//! to `soma_infra::web::serve_spa::<Portal>`.

use rust_embed::RustEmbed;

/// Compile-time embedded assets from the Trunk build output directory.
///
/// Build the dashboard with:
///   cd dashboard && trunk build --release
/// to populate `dashboard/dist/` before `cargo build`.
///
/// A placeholder `index.html` lives in `dashboard/dist/` so `cargo build`
/// always succeeds even without a Trunk build.
#[derive(RustEmbed)]
#[folder = "dashboard/dist"]
pub struct Portal;
