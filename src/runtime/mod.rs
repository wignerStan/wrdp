//! Runtime support surface.
//!
//! Exposes best-effort startup diagnostics used by CLI/server runtime flows.

pub mod capability_report;
pub mod diagnostics;
pub(crate) mod shutdown;

pub use capability_report::{print_capabilities, print_diagnostics};
pub use diagnostics::{SystemInfo, log_startup_diagnostics};
