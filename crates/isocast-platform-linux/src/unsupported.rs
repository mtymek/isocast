//! The error for what the Linux platform can't do yet.

use anyhow::anyhow;

/// The error every operation not built yet returns.
pub fn unsupported(what: &str) -> anyhow::Error {
    anyhow!("{what}: not supported on Linux yet (see PLAN_LINUX.md)")
}
