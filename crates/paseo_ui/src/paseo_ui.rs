//! pazedo's Paseo UI: a sidebar listing a Paseo daemon's workspaces in place of Zed's threads
//! sidebar, and Paseo agent sessions shown with Zed's thread UI.

mod agent_item;
mod agent_session;
mod sidebar;
mod store;

#[cfg(test)]
mod paseo_ui_tests;

pub use agent_item::PaseoAgentItem;
pub use agent_session::{
    MODEL_ENV_VAR, NewAgent, PROVIDER_ENV_VAR, PaseoAgentServer, PaseoConnection,
};
pub use sidebar::PaseoSidebar;
pub use store::{
    ConnectionStatus, ENDPOINT_ENV_VAR, PaseoStore, PaseoStoreEvent, ProjectGroup, WorkspaceEntry,
};
