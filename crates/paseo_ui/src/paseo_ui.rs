//! pazedo's Paseo UI: a sidebar listing a Paseo daemon's workspaces in place of Zed's threads
//! sidebar.

mod sidebar;
mod store;

#[cfg(test)]
mod paseo_ui_tests;

pub use sidebar::PaseoSidebar;
pub use store::{ConnectionStatus, ENDPOINT_ENV_VAR, PaseoStore, ProjectGroup, WorkspaceEntry};
