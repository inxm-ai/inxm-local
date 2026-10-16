//! Build identity shown in the desktop UI.

/// Git tag (or commit description) captured when this binary was built.
/// Falls back to Cargo's package version when Git metadata is unavailable.
pub const DISPLAY: &str = env!("INXM_BUILD_VERSION");
