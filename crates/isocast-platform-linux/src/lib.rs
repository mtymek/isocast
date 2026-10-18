//! Linux back end for Isocast. Empty on other systems, so the workspace
//! still builds everywhere.

#[cfg(target_os = "linux")]
pub mod plugin;
