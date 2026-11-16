//! pazedo's Paseo UI: a sidebar listing a Paseo daemon's workspaces in place of Zed's threads
//! sidebar, Paseo agent sessions shown with Zed's thread UI, and Paseo terminals shown with
//! Zed's terminal view.

mod add_project;
mod agent_images;
mod agent_item;
mod agent_session;
mod daemon_auth;
mod daemon_settings;
mod new_workspace;
mod panel;
mod rename_prompt;
mod sidebar;
mod store;
mod terminal_item;
mod usage;

#[cfg(test)]
mod paseo_ui_tests;

pub use agent_item::PaseoAgentItem;
pub use agent_session::{
    MODEL_ENV_VAR, NewAgent, PROVIDER_ENV_VAR, PaseoAgentServer, PaseoConnection,
};
pub use daemon_settings::ManageDaemons;
pub use panel::{PaseoPanel, REPLACES_ZED_AGENT_PANEL, ToggleFocus};
pub use sidebar::{Grouping, PaseoSidebar};
pub use store::{
    ConnectionStatus, DaemonConfig, ENDPOINT_ENV_VAR, PaseoDaemons, PaseoStore, PaseoStoreEvent,
    ProjectGroup, SSH_ENV_VAR, WorkspaceEntry,
};
pub use terminal_item::PaseoTerminalItem;

/// Registers the workspace sidebar's window actions and shows the AI subscriptions' usage in
/// every window's title bar. Call before windows open.
pub fn init(cx: &mut gpui::App) {
    agent_item::init(cx);
    terminal_item::init(cx);
    sidebar::init(cx);
    usage::init(cx);
}
