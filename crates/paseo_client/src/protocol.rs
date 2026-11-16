//! Hand-written serde types for the subset of the Paseo daemon protocol that pazedo uses.
//!
//! The source of truth is the Zod schemas in `@getpaseo/protocol` (`dist/messages.js`), pinned to
//! [`PASEO_PROTOCOL_PACKAGE_VERSION`]. The protocol is 0.x and changes often, so these types are
//! deliberately lenient: unknown fields are ignored, enums have an `Unknown` fallback, and
//! messages that are not modelled here are surfaced as raw JSON instead of failing.

use std::time::Duration;

use collections::BTreeMap;
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::Value;

pub const PASEO_PROTOCOL_PACKAGE_VERSION: &str = "0.15.4";
pub const PROTOCOL_VERSION: u32 = 1;

/// A session request that the daemon answers with exactly one correlated response.
///
/// The client adds `type` and `requestId` when sending, and the daemon echoes `requestId` inside
/// the response payload.
pub trait Request: Serialize {
    const TYPE: &'static str;
    const RESPONSE_TYPE: &'static str;
    type Response: DeserializeOwned;
    /// How long the response may take, when longer than the client's usual timeout.
    const TIMEOUT: Option<Duration> = None;
}

/// A session request that the daemon answers with any number of `UPDATE_TYPE` messages carrying
/// its `requestId`, then its response.
pub trait StreamingRequest: Request {
    const UPDATE_TYPE: &'static str;
    type Update: DeserializeOwned;
}

/// A session message the daemon does not answer.
pub trait Notification: Serialize {
    const TYPE: &'static str;
}

/// Paseo's own desktop, web and mobile apps all identify as `Mobile`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientType {
    Mobile,
    Browser,
    Cli,
    Mcp,
    Hub,
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum HelloAuth {
    Password { password: String },
    LocalCredential { token: String },
}

/// Leaves the secret out, so that logging a value never records it.
impl std::fmt::Debug for HelloAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Password { .. } => formatter.write_str("Password { .. }"),
            Self::LocalCredential { .. } => formatter.write_str("LocalCredential { .. }"),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Hello<'a> {
    #[serde(rename = "type")]
    pub message_type: &'static str,
    pub client_id: &'a str,
    pub client_type: ClientType,
    pub protocol_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<&'a HelloAuth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_version: Option<&'a str>,
    pub capabilities: &'a BTreeMap<String, bool>,
}

/// Wire names of the hello capabilities pazedo understands (`CLIENT_CAPS` in the protocol).
pub mod capabilities {
    pub const HELLO_REJECTION: &str = "hello_rejection";
    pub const OWNED_SUBSCRIPTIONS: &str = "owned_subscriptions";
    pub const EXPLICIT_EVENT_SUBSCRIPTIONS: &str = "explicit_event_subscriptions";
    pub const PROJECT_UPDATES: &str = "project_updates";
    pub const ALL_PROVIDERS: &str = "all_providers";
    pub const BINARY_CHUNKING_V1: &str = "binary_chunking_v1";
    pub const SELECTIVE_AGENT_TIMELINE: &str = "selective_agent_timeline";
}

/// The `server_info` status the daemon sends after accepting `hello`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub protocol_version: Option<u32>,
    pub server_id: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub hostname: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub version: Option<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub features: BTreeMap<String, Value>,
}

impl ServerInfo {
    pub fn has_feature(&self, name: &str) -> bool {
        self.features.get(name) == Some(&Value::Bool(true))
    }
}

/// Both fields are `z.unknown()` transformed to a trimmed string or null on the daemon side.
fn lenient_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::String(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
        _ => None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelloRejectionReason {
    PasswordRequired,
    IncorrectPassword,
    IncompatibleProtocol,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcErrorPayload {
    pub request_id: String,
    pub request_type: Option<String>,
    pub error: String,
    pub code: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Directory: projects, workspaces, agents
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStatus {
    NeedsInput,
    Failed,
    Running,
    Attention,
    Done,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectKind {
    Git,
    NonGit,
    Directory,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    Directory,
    LocalCheckout,
    Checkout,
    Worktree,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub id: String,
    pub project_id: String,
    pub project_display_name: String,
    pub project_root_path: String,
    workspace_directory: Option<String>,
    pub project_kind: ProjectKind,
    pub workspace_kind: WorkspaceKind,
    /// The resolved display name: the user's title if set, otherwise the branch or directory.
    pub name: String,
    pub title: Option<String>,
    pub worktree_slug: Option<String>,
    pub pinned_at: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub background: bool,
    pub archiving_at: Option<String>,
    pub status: WorkspaceStatus,
    pub status_entered_at: Option<String>,
    pub activity_at: Option<String>,
    pub created_at: Option<String>,
    pub last_user_message_at: Option<String>,
    pub diff_stat: Option<DiffStat>,
    pub git_runtime: Option<GitRuntime>,
    pub github_runtime: Option<GithubRuntime>,
}

impl Workspace {
    /// The directory to open as the project. Old daemons omit `workspaceDirectory`.
    pub fn directory(&self) -> &str {
        self.workspace_directory
            .as_deref()
            .unwrap_or(&self.project_root_path)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DiffStat {
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitRuntime {
    pub current_branch: Option<String>,
    pub remote_url: Option<String>,
    pub is_paseo_owned_worktree: Option<bool>,
    pub is_dirty: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubRuntime {
    pub pull_request: Option<PullRequest>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: Option<u64>,
    pub url: String,
    pub title: String,
    /// GitHub's state, e.g. `OPEN`, `CLOSED` or `MERGED`.
    pub state: String,
    #[serde(default)]
    pub is_merged: bool,
    pub is_draft: Option<bool>,
    /// The overall state of the pull request's CI checks.
    pub checks_status: Option<ChecksStatus>,
    #[serde(default)]
    pub checks: Vec<PullRequestCheck>,
    pub review_decision: Option<ReviewDecision>,
    pub mergeable: Option<Mergeable>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksStatus {
    None,
    Pending,
    Success,
    Failure,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestCheck {
    pub name: String,
    pub status: CheckStatus,
    pub url: Option<String>,
    pub workflow: Option<String>,
    pub duration: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Success,
    Failure,
    Pending,
    Skipped,
    Cancelled,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    Pending,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Mergeable {
    Mergeable,
    Conflicting,
    #[serde(other)]
    Unknown,
}

/// A project as listed by `project.list.request`, or a project with no active workspaces.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSummary {
    pub project_id: String,
    pub project_key: Option<String>,
    pub project_display_name: String,
    pub project_root_path: String,
    pub project_kind: ProjectKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Initializing,
    Idle,
    Running,
    Error,
    Closed,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionReason {
    Finished,
    Error,
    Permission,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    pub id: String,
    pub provider: String,
    pub cwd: String,
    pub workspace_id: Option<String>,
    pub model: Option<String>,
    pub thinking_option_id: Option<String>,
    /// The thinking level in effect, after the model's default fills in an unset one.
    pub effective_thinking_option_id: Option<String>,
    pub title: Option<String>,
    pub status: AgentStatus,
    pub created_at: String,
    pub updated_at: String,
    pub last_user_message_at: Option<String>,
    pub active_turn: Option<ActiveTurn>,
    pub current_mode_id: Option<String>,
    #[serde(default)]
    pub available_modes: Vec<AgentMode>,
    #[serde(default)]
    pub pending_permissions: Vec<PermissionRequest>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub requires_attention: bool,
    pub attention_reason: Option<AttentionReason>,
    pub attention_timestamp: Option<String>,
    pub archived_at: Option<String>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub provider_unavailable: bool,
    /// The context window and cost as of the latest turn.
    #[serde(default)]
    pub last_usage: Option<AgentUsage>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentUsage {
    #[serde(default)]
    pub input_tokens: Option<f64>,
    #[serde(default)]
    pub output_tokens: Option<f64>,
    #[serde(default)]
    pub total_cost_usd: Option<f64>,
    #[serde(default)]
    pub context_window_max_tokens: Option<f64>,
    #[serde(default)]
    pub context_window_used_tokens: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AgentMode {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveTurn {
    pub turn_id: String,
    pub started_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    Tool,
    Plan,
    Question,
    Mode,
    Other,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    pub id: String,
    pub provider: String,
    pub name: String,
    pub kind: PermissionKind,
    pub title: Option<String>,
    pub description: Option<String>,
    pub input: Option<Value>,
    pub detail: Option<Value>,
    #[serde(default)]
    pub actions: Vec<PermissionAction>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PermissionAction {
    pub id: String,
    pub label: String,
    pub behavior: PermissionBehavior,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionBehavior {
    Allow,
    Deny,
}

/// Where an agent lives, attached to directory entries and `agent_update` upserts.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectPlacement {
    pub project_key: String,
    pub project_name: String,
    pub workspace_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub next_cursor: Option<String>,
    pub prev_cursor: Option<String>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub limit: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// Asks the daemon to keep pushing updates for the listed entities after the response.
///
/// The daemon assigns the subscription id (in the response) and rejects client-chosen ones.
/// Subscriptions belong to one connection: after a reconnect they must be requested again.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Subscribe {}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchWorkspaces {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_background: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<Page>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscribe: Option<Subscribe>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchWorkspacesResponse {
    pub subscription_id: Option<String>,
    pub entries: Vec<Workspace>,
    #[serde(default)]
    pub empty_projects: Vec<ProjectSummary>,
    pub page_info: PageInfo,
}

impl Request for FetchWorkspaces {
    const TYPE: &'static str = "fetch_workspaces_request";
    const RESPONSE_TYPE: &'static str = "fetch_workspaces_response";
    type Response = FetchWorkspacesResponse;
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentFilter {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_archived: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_attention: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_background: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchAgents {
    /// `Active` drops archived agents and agents whose workspace is not active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<AgentScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_background: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<AgentFilter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<Page>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscribe: Option<Subscribe>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentScope {
    Active,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEntry {
    pub agent: Agent,
    pub project: Option<ProjectPlacement>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchAgentsResponse {
    pub subscription_id: Option<String>,
    pub entries: Vec<AgentEntry>,
    pub page_info: PageInfo,
}

impl Request for FetchAgents {
    const TYPE: &'static str = "fetch_agents_request";
    const RESPONSE_TYPE: &'static str = "fetch_agents_response";
    type Response = FetchAgentsResponse;
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ListProjects {}

#[derive(Debug, Clone, Deserialize)]
pub struct ListProjectsResponse {
    pub projects: Vec<ProjectSummary>,
}

impl Request for ListProjects {
    const TYPE: &'static str = "project.list.request";
    const RESPONSE_TYPE: &'static str = "project.list.response";
    type Response = ListProjectsResponse;
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseSubscription {
    pub subscription_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseSubscriptionResponse {
    pub subscription_id: String,
}

impl Request for ReleaseSubscription {
    const TYPE: &'static str = "subscription.release.request";
    const RESPONSE_TYPE: &'static str = "subscription.release.response";
    type Response = ReleaseSubscriptionResponse;
}

#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum WorkspaceUpdate {
    Upsert {
        subscription_id: Option<String>,
        workspace: Workspace,
    },
    Remove {
        subscription_id: Option<String>,
        id: String,
        /// Set when removing this workspace left its project without active workspaces.
        empty_project: Option<ProjectSummary>,
        removed_project_id: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AgentUpdate {
    Upsert {
        subscription_id: Option<String>,
        agent: Agent,
        project: Option<ProjectPlacement>,
    },
    Remove {
        subscription_id: Option<String>,
        agent_id: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ProjectUpdate {
    Upsert {
        subscription_id: Option<String>,
        project: ProjectSummary,
    },
    Remove {
        subscription_id: Option<String>,
        project_id: String,
    },
}

/// Subscribes to discrete events that have no directory subscription of their own, such as
/// `project.update`, `agent_attention_required` and `agent_permission_request`. Names must be in
/// the daemon's `SessionEventSubscriptionSchema`, or the whole request is rejected.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionEventSubscription {
    pub events: Vec<String>,
    /// Ask for `shouldNotify` on attention events. Only one connection gets notifications.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notifications: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionEventSubscriptionResponse {
    pub subscription_id: Option<String>,
}

impl Request for SetSessionEventSubscription {
    const TYPE: &'static str = "session.events.set_subscription.request";
    const RESPONSE_TYPE: &'static str = "session.events.set_subscription.response";
    type Response = SetSessionEventSubscriptionResponse;
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAttentionRequired {
    pub subscription_id: Option<String>,
    pub agent_id: String,
    pub reason: AttentionReason,
    pub timestamp: String,
    pub should_notify: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPermissionRequested {
    pub subscription_id: Option<String>,
    pub agent_id: String,
    pub request: PermissionRequest,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPermissionResolved {
    pub subscription_id: Option<String>,
    pub agent_id: String,
    /// The id of the permission request that was resolved, not a client request id.
    pub request_id: String,
    pub resolution: Value,
}

// ---------------------------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActiveTurnBehavior {
    Interrupt,
    Steer,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendAgentMessage {
    /// A full id, a unique prefix, or an exact title; the daemon resolves it.
    pub agent_id: String,
    pub text: String,
    /// Client-chosen id the daemon uses to deduplicate retried sends.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_turn_behavior: Option<ActiveTurnBehavior>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageAttachment>,
}

/// An image sent with a message.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageAttachment {
    /// Base64-encoded.
    pub data: String,
    /// e.g. "image/png".
    pub mime_type: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendAgentMessageResponse {
    pub agent_id: String,
    pub accepted: bool,
    pub error: Option<String>,
}

impl Request for SendAgentMessage {
    const TYPE: &'static str = "send_agent_message_request";
    const RESPONSE_TYPE: &'static str = "send_agent_message_response";
    type Response = SendAgentMessageResponse;
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelAgent {
    pub agent_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelAgentResponse {
    pub agent_id: String,
    pub agent: Option<Agent>,
    pub error: Option<String>,
}

impl Request for CancelAgent {
    const TYPE: &'static str = "cancel_agent_request";
    const RESPONSE_TYPE: &'static str = "cancel_agent_response";
    type Response = CancelAgentResponse;
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionConfig {
    pub provider: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Creates an agent in an existing workspace (or the workspace for `cwd`) and optionally starts
/// its first turn.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateAgent {
    pub config: AgentSessionConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateAgentResponse {
    pub agent: Option<Agent>,
    pub error: Option<String>,
}

impl Request for CreateAgent {
    const TYPE: &'static str = "agent.create.request";
    const RESPONSE_TYPE: &'static str = "agent.create.response";
    type Response = CreateAgentResponse;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "behavior",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PermissionResponse {
    Allow {
        #[serde(skip_serializing_if = "Option::is_none")]
        selected_action_id: Option<String>,
    },
    Deny {
        #[serde(skip_serializing_if = "Option::is_none")]
        selected_action_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        interrupt: Option<bool>,
    },
}

/// Answers a pending permission request. `request_id` is the permission request's own id
/// ([`PermissionRequest::id`]); the daemon announces the outcome with `agent_permission_resolved`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RespondToPermission {
    pub agent_id: String,
    pub request_id: String,
    pub response: PermissionResponse,
}

impl Notification for RespondToPermission {
    const TYPE: &'static str = "agent_permission_response";
}

/// One `AgentTimelineItemPayload`. History entries carry merged items; live `timeline` stream
/// events carry deltas (an `assistant_message` event holds the next chunk of text).
#[derive(Debug, Clone, PartialEq)]
pub enum TimelineItem {
    UserMessage {
        text: String,
        message_id: Option<String>,
    },
    AssistantMessage {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall(ToolCall),
    Todo {
        items: Vec<TodoItem>,
    },
    Error {
        message: String,
    },
    Notification {
        level: String,
        message: String,
    },
    /// Compaction markers, plugin items and anything newer than this crate.
    Other(Value),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub status: ToolCallStatus,
    #[serde(default)]
    pub detail: Value,
    pub error: Option<Value>,
    /// Set when the item is a message between agents rather than a tool call: the prompt an
    /// agent received from the agent that started it (`message`), or news about an agent this
    /// one started (`finished`, `errored`, ...). Paseo stores both as `agent_message` tool calls.
    pub agent_message: Option<AgentMessage>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AgentMessage {
    pub event: AgentMessageEvent,
    pub sender: AgentMessageSender,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentMessageEvent {
    Message,
    Finished,
    Errored,
    PermissionRequired,
    Closed,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessageSender {
    pub id: String,
    pub server_id: Option<String>,
    pub title: Option<String>,
}

impl ToolCall {
    pub fn detail(&self) -> ToolCallDetail {
        ToolCallDetail::from_value(&self.detail)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Running,
    Completed,
    Failed,
    Canceled,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolCallDetail {
    Shell {
        command: String,
        cwd: Option<String>,
        output: Option<String>,
        exit_code: Option<i64>,
    },
    Read {
        file_path: String,
        content: Option<String>,
    },
    Edit {
        file_path: String,
        old_string: Option<String>,
        new_string: Option<String>,
        unified_diff: Option<String>,
    },
    Write {
        file_path: String,
        content: Option<String>,
    },
    Search {
        query: String,
        content: Option<String>,
    },
    Fetch {
        url: String,
        result: Option<String>,
    },
    SubAgent {
        description: Option<String>,
        child_session_id: Option<String>,
    },
    PlainText {
        label: Option<String>,
        text: Option<String>,
    },
    Plan {
        text: String,
    },
    Other(Value),
}

impl ToolCallDetail {
    fn from_value(value: &Value) -> Self {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        let required = |key: &str| text(key).unwrap_or_default();
        match value.get("type").and_then(Value::as_str) {
            Some("shell") => Self::Shell {
                command: required("command"),
                cwd: text("cwd"),
                output: text("output"),
                exit_code: value.get("exitCode").and_then(Value::as_i64),
            },
            Some("read") => Self::Read {
                file_path: required("filePath"),
                content: text("content"),
            },
            Some("edit") => Self::Edit {
                file_path: required("filePath"),
                old_string: text("oldString"),
                new_string: text("newString"),
                unified_diff: text("unifiedDiff"),
            },
            Some("write") => Self::Write {
                file_path: required("filePath"),
                content: text("content"),
            },
            Some("search") => Self::Search {
                query: required("query"),
                content: text("content"),
            },
            Some("fetch") => Self::Fetch {
                url: required("url"),
                result: text("result"),
            },
            Some("sub_agent") => Self::SubAgent {
                description: text("description"),
                child_session_id: text("childSessionId"),
            },
            Some("plain_text") => Self::PlainText {
                label: text("label"),
                text: text("text"),
            },
            Some("plan") => Self::Plan {
                text: required("text"),
            },
            _ => Self::Other(value.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem {
    pub text: String,
    pub completed: bool,
    pub status: Option<String>,
}

impl TimelineItem {
    pub fn from_value(value: &Value) -> Self {
        #[derive(Deserialize)]
        #[serde(
            tag = "type",
            rename_all = "snake_case",
            rename_all_fields = "camelCase"
        )]
        enum Known {
            UserMessage {
                text: String,
                message_id: Option<String>,
            },
            AssistantMessage {
                text: String,
            },
            Reasoning {
                text: String,
            },
            Todo {
                items: Vec<TodoItem>,
            },
            Error {
                message: String,
            },
            Notification {
                level: String,
                message: String,
            },
        }

        if value.get("type").and_then(Value::as_str) == Some("tool_call") {
            return match serde_json::from_value::<ToolCall>(value.clone()) {
                Ok(tool_call) => Self::ToolCall(tool_call),
                Err(_) => Self::Other(value.clone()),
            };
        }
        match serde_json::from_value::<Known>(value.clone()) {
            Ok(Known::UserMessage { text, message_id }) => Self::UserMessage { text, message_id },
            Ok(Known::AssistantMessage { text }) => Self::AssistantMessage { text },
            Ok(Known::Reasoning { text }) => Self::Reasoning { text },
            Ok(Known::Todo { items }) => Self::Todo { items },
            Ok(Known::Error { message }) => Self::Error { message },
            Ok(Known::Notification { level, message }) => Self::Notification { level, message },
            Err(_) => Self::Other(value.clone()),
        }
    }
}

/// One `AgentStreamEventPayload` pushed on an agent timeline subscription.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentStreamEvent {
    TurnStarted {
        turn_id: Option<String>,
    },
    TurnCompleted {
        turn_id: Option<String>,
    },
    TurnFailed {
        turn_id: Option<String>,
        error: String,
    },
    TurnCanceled {
        turn_id: Option<String>,
        reason: String,
    },
    Timeline {
        item: TimelineItem,
        turn_id: Option<String>,
    },
    PermissionRequested(PermissionRequest),
    PermissionResolved {
        request_id: String,
        resolution: Value,
    },
    AttentionRequired {
        reason: AttentionReason,
    },
    /// `thread_started` and anything newer than this crate.
    Other(Value),
}

impl AgentStreamEvent {
    pub fn from_value(value: &Value) -> Self {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        match value.get("type").and_then(Value::as_str) {
            Some("turn_started") => Self::TurnStarted {
                turn_id: text("turnId"),
            },
            Some("turn_completed") => Self::TurnCompleted {
                turn_id: text("turnId"),
            },
            Some("turn_failed") => Self::TurnFailed {
                turn_id: text("turnId"),
                error: text("error").unwrap_or_default(),
            },
            Some("turn_canceled") => Self::TurnCanceled {
                turn_id: text("turnId"),
                reason: text("reason").unwrap_or_default(),
            },
            Some("timeline") => match value.get("item") {
                Some(item) => Self::Timeline {
                    item: TimelineItem::from_value(item),
                    turn_id: text("turnId"),
                },
                None => Self::Other(value.clone()),
            },
            Some("permission_requested") => {
                match value
                    .get("request")
                    .cloned()
                    .map(serde_json::from_value::<PermissionRequest>)
                {
                    Some(Ok(request)) => Self::PermissionRequested(request),
                    _ => Self::Other(value.clone()),
                }
            }
            Some("permission_resolved") => Self::PermissionResolved {
                request_id: text("requestId").unwrap_or_default(),
                resolution: value.get("resolution").cloned().unwrap_or_default(),
            },
            Some("attention_required") => {
                match value
                    .get("reason")
                    .cloned()
                    .map(serde_json::from_value::<AttentionReason>)
                {
                    Some(Ok(reason)) => Self::AttentionRequired { reason },
                    _ => Self::Other(value.clone()),
                }
            }
            _ => Self::Other(value.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineCursor {
    pub epoch: String,
    pub seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineDirection {
    Tail,
    Before,
    After,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineProjection {
    Projected,
    Canonical,
}

/// Fetches a page of an agent's timeline history. The app uses `tail` with `projected` and
/// `whole_turns`, then `after` from the last cursor to catch up after gaps or reconnects.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchAgentTimeline {
    pub agent_id: String,
    pub direction: TimelineDirection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<TimelineCursor>,
    /// 0 means all rows in the window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    pub projection: TimelineProjection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub whole_turns: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineEntry {
    /// One `AgentTimelineItemPayload` (message, reasoning, tool call, ...), kept as JSON until
    /// pazedo renders timelines.
    pub item: Value,
    pub turn_id: Option<String>,
    pub timestamp: String,
    pub seq_start: u64,
    pub seq_end: u64,
}

impl TimelineEntry {
    pub fn item_type(&self) -> Option<&str> {
        self.item.get("type").and_then(Value::as_str)
    }

    pub fn item(&self) -> TimelineItem {
        TimelineItem::from_value(&self.item)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchAgentTimelineResponse {
    pub agent_id: String,
    pub agent: Option<Agent>,
    pub epoch: String,
    /// The client's cached rows are invalid; rebuild from this response.
    pub reset: bool,
    pub stale_cursor: bool,
    pub gap: bool,
    pub start_cursor: Option<TimelineCursor>,
    pub end_cursor: Option<TimelineCursor>,
    pub has_older: bool,
    pub has_newer: bool,
    pub entries: Vec<TimelineEntry>,
    pub error: Option<String>,
}

impl Request for FetchAgentTimeline {
    const TYPE: &'static str = "fetch_agent_timeline_request";
    const RESPONSE_TYPE: &'static str = "fetch_agent_timeline_response";
    type Response = FetchAgentTimelineResponse;
}

/// Starts live `agent_stream` delivery for these agents. Each request is a separate
/// subscription; release it with [`ReleaseSubscription`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetAgentTimelineSubscription {
    pub agent_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetAgentTimelineSubscriptionResponse {
    pub subscription_id: Option<String>,
    pub agent_ids: Vec<String>,
}

impl Request for SetAgentTimelineSubscription {
    const TYPE: &'static str = "agent.timeline.set_subscription.request";
    const RESPONSE_TYPE: &'static str = "agent.timeline.set_subscription.response";
    type Response = SetAgentTimelineSubscriptionResponse;
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStream {
    pub subscription_id: Option<String>,
    pub agent_id: String,
    /// One `AgentStreamEventPayload`: `turn_started`, `timeline`, `turn_completed`, ...
    pub event: Value,
    pub timestamp: String,
    /// Present for timeline rows; consecutive per epoch, so a jump means a gap to fetch.
    pub seq: Option<u64>,
    pub epoch: Option<String>,
}

impl AgentStream {
    pub fn event_type(&self) -> Option<&str> {
        self.event.get("type").and_then(Value::as_str)
    }

    pub fn event(&self) -> AgentStreamEvent {
        AgentStreamEvent::from_value(&self.event)
    }
}

/// The agent's history was replaced (for example by a rewind); refetch from the tail.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTimelineReplacement {
    pub subscription_id: Option<String>,
    pub agent_id: String,
    pub epoch: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetAgentModel {
    pub agent_id: String,
    /// `None` returns to the provider's default model.
    pub model_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentActionResponse {
    pub agent_id: String,
    pub accepted: bool,
    pub error: Option<String>,
}

impl Request for SetAgentModel {
    const TYPE: &'static str = "set_agent_model_request";
    const RESPONSE_TYPE: &'static str = "set_agent_model_response";
    type Response = AgentActionResponse;
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetAgentMode {
    pub agent_id: String,
    pub mode_id: String,
}

impl Request for SetAgentMode {
    const TYPE: &'static str = "set_agent_mode_request";
    const RESPONSE_TYPE: &'static str = "set_agent_mode_response";
    type Response = AgentActionResponse;
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetAgentThinking {
    pub agent_id: String,
    /// `None` returns to the model's default thinking level.
    pub thinking_option_id: Option<String>,
}

impl Request for SetAgentThinking {
    const TYPE: &'static str = "set_agent_thinking_request";
    const RESPONSE_TYPE: &'static str = "set_agent_thinking_response";
    type Response = AgentActionResponse;
}

/// The providers the daemon can run, with their models and modes. Without the
/// `compact_provider_snapshots` capability the daemon answers with full entries.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetProvidersSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProvidersSnapshot {
    pub entries: Vec<ProviderEntry>,
}

impl Request for GetProvidersSnapshot {
    const TYPE: &'static str = "get_providers_snapshot_request";
    const RESPONSE_TYPE: &'static str = "get_providers_snapshot_response";
    type Response = ProvidersSnapshot;
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderEntry {
    pub provider: String,
    pub status: ProviderStatus,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub label: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub models: Vec<ProviderModel>,
    #[serde(default)]
    pub modes: Vec<AgentMode>,
    pub default_mode_id: Option<String>,
}

impl ProviderEntry {
    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.provider)
    }

    pub fn default_model(&self) -> Option<&ProviderModel> {
        self.models
            .iter()
            .find(|model| model.is_default)
            .or_else(|| self.models.first())
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatus {
    Ready,
    Loading,
    Error,
    Unavailable,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModel {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    #[serde(default)]
    pub is_default: bool,
    /// Hidden aliases and legacy ids are not offered for selection.
    pub is_selectable: Option<bool>,
    #[serde(default)]
    pub thinking_options: Vec<SelectOption>,
    pub default_thinking_option_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    #[serde(default)]
    pub is_default: bool,
}

// ---------------------------------------------------------------------------------------------
// Terminals
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInfo {
    pub id: String,
    pub name: String,
    pub cwd: Option<String>,
    pub workspace_id: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListTerminals {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListTerminalsResponse {
    pub cwd: Option<String>,
    pub terminals: Vec<TerminalInfo>,
}

impl Request for ListTerminals {
    const TYPE: &'static str = "list_terminals_request";
    const RESPONSE_TYPE: &'static str = "list_terminals_response";
    type Response = ListTerminalsResponse;
}

/// Subscribes to the terminal list of one directory. The first `terminals_changed` carries the
/// request id and acts as the response; later ones arrive as events.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeTerminals {
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalsChanged {
    pub subscription_id: Option<String>,
    pub workspace_id: Option<String>,
    pub cwd: String,
    pub terminals: Vec<TerminalInfo>,
}

impl Request for SubscribeTerminals {
    const TYPE: &'static str = "subscribe_terminals_request";
    const RESPONSE_TYPE: &'static str = "terminals_changed";
    type Response = TerminalsChanged;
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnsubscribeTerminals {
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

impl Notification for UnsubscribeTerminals {
    const TYPE: &'static str = "unsubscribe_terminals_request";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTerminal {
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<TerminalSize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateTerminalResponse {
    pub terminal: Option<TerminalInfo>,
    pub error: Option<String>,
}

impl Request for CreateTerminal {
    const TYPE: &'static str = "create_terminal_request";
    const RESPONSE_TYPE: &'static str = "create_terminal_response";
    type Response = CreateTerminalResponse;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalRestoreMode {
    Live,
    VisibleSnapshot,
    FullSnapshot,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalRestore {
    pub mode: TerminalRestoreMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scrollback_lines: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<TerminalSize>,
}

/// Attaches to one terminal's output. The daemon assigns a slot; binary frames for this
/// terminal carry that slot.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeTerminal {
    pub terminal_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restore: Option<TerminalRestore>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeTerminalResponse {
    pub terminal_id: String,
    pub slot: Option<u8>,
    pub subscription_id: Option<String>,
    pub error: Option<String>,
}

impl Request for SubscribeTerminal {
    const TYPE: &'static str = "subscribe_terminal_request";
    const RESPONSE_TYPE: &'static str = "subscribe_terminal_response";
    type Response = SubscribeTerminalResponse;
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnsubscribeTerminal {
    pub terminal_id: String,
}

impl Notification for UnsubscribeTerminal {
    const TYPE: &'static str = "unsubscribe_terminal_request";
}

/// Whether a resize takes over the terminal's size (`claim`) or only applies while this client
/// owns it (`update`). The last client to claim decides the size every client sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalResizeIntent {
    Claim,
    Update,
}

/// The JSON payload of a binary resize frame ([`crate::frames::TerminalOpcode::Resize`]).
#[derive(Debug, Clone, Serialize)]
pub struct TerminalStreamResize {
    pub rows: u16,
    pub cols: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<TerminalResizeIntent>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalClientMessage {
    Input { data: String },
    Resize { rows: u16, cols: u16 },
}

/// Terminal input and resize as JSON. Subscribed terminals can also take input as binary
/// frames on their slot (see [`crate::frames::TerminalFrame`]).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInput {
    pub terminal_id: String,
    pub message: TerminalClientMessage,
}

impl Notification for TerminalInput {
    const TYPE: &'static str = "terminal_input";
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KillTerminal {
    pub terminal_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KillTerminalResponse {
    pub terminal_id: String,
    pub success: bool,
}

impl Request for KillTerminal {
    const TYPE: &'static str = "kill_terminal_request";
    const RESPONSE_TYPE: &'static str = "kill_terminal_response";
    type Response = KillTerminalResponse;
}

/// Reloads a closed agent: the daemon resumes its provider session and rebuilds its timeline,
/// as "Reload agent" in Paseo's apps does.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshAgent {
    pub agent_id: String,
}

/// The daemon answers with a `status` message (`agent_refreshed`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshAgentResponse {
    pub status: String,
    pub agent_id: Option<String>,
}

impl Request for RefreshAgent {
    const TYPE: &'static str = "refresh_agent_request";
    const RESPONSE_TYPE: &'static str = "status";
    type Response = RefreshAgentResponse;
}

/// Marks an agent's finished turn or error as seen, which turns its workspace from `attention`
/// to `done`. Paseo's apps send it when the user views the agent.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearAgentAttention {
    pub agent_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearAgentAttentionResponse {
    #[serde(default)]
    pub agents: Vec<Agent>,
}

impl Request for ClearAgentAttention {
    const TYPE: &'static str = "clear_agent_attention";
    const RESPONSE_TYPE: &'static str = "clear_agent_attention_response";
    type Response = ClearAgentAttentionResponse;
}

/// Archives a workspace as Paseo's apps do: its agents are archived, its terminals killed, and a
/// Paseo worktree no other workspace uses is deleted from disk.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveWorkspace {
    pub workspace_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveWorkspaceResponse {
    pub archived_at: Option<String>,
    pub error: Option<String>,
    /// An undo from another client stopped the archive.
    #[serde(default)]
    pub canceled: bool,
}

impl Request for ArchiveWorkspace {
    const TYPE: &'static str = "archive_workspace_request";
    const RESPONSE_TYPE: &'static str = "archive_workspace_response";
    type Response = ArchiveWorkspaceResponse;
}

/// Sets a workspace's title; `None` reverts its name to the one derived from its branch or
/// directory.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetWorkspaceTitle {
    pub workspace_id: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetWorkspaceTitleResponse {
    pub accepted: bool,
    pub title: Option<String>,
    pub error: Option<String>,
}

impl Request for SetWorkspaceTitle {
    const TYPE: &'static str = "workspace.title.set.request";
    const RESPONSE_TYPE: &'static str = "workspace.title.set.response";
    type Response = SetWorkspaceTitleResponse;
}

/// Renames an agent.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameAgent {
    pub agent_id: String,
    pub name: String,
}

impl Request for RenameAgent {
    const TYPE: &'static str = "update_agent_request";
    const RESPONSE_TYPE: &'static str = "update_agent_response";
    type Response = AgentActionResponse;
}

/// Renames a terminal. The title then stays until the next rename, instead of following the
/// shell's.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameTerminal {
    pub terminal_id: String,
    pub title: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RenameTerminalResponse {
    pub success: bool,
    #[serde(default)]
    pub error: Option<String>,
}

impl Request for RenameTerminal {
    const TYPE: &'static str = "terminal.rename.request";
    const RESPONSE_TYPE: &'static str = "terminal.rename.response";
    type Response = RenameTerminalResponse;
}

/// Creates a workspace: always a new one, never a lookup by directory.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorkspace {
    pub source: WorkspaceSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WorkspaceSource {
    /// The project's existing checkout.
    #[serde(rename_all = "camelCase")]
    Directory {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
    },
    /// A new git worktree of the project, on a new branch off `ref_name` (the daemon names the
    /// branch unless `branch_name` is given).
    #[serde(rename_all = "camelCase")]
    Worktree {
        cwd: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
        action: WorktreeAction,
        #[serde(skip_serializing_if = "Option::is_none")]
        ref_name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        branch_name: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorktreeAction {
    BranchOff,
    Checkout,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorkspaceResponse {
    pub workspace: Option<Workspace>,
    pub setup_terminal_id: Option<String>,
    pub error: Option<String>,
}

impl Request for CreateWorkspace {
    const TYPE: &'static str = "workspace.create.request";
    const RESPONSE_TYPE: &'static str = "workspace.create.response";
    type Response = CreateWorkspaceResponse;
}

/// Branches of the repository at `cwd`, most recently committed first.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSuggestions {
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSuggestionsResponse {
    #[serde(default)]
    pub branches: Vec<String>,
    pub error: Option<String>,
}

impl Request for BranchSuggestions {
    const TYPE: &'static str = "branch_suggestions_request";
    const RESPONSE_TYPE: &'static str = "branch_suggestions_response";
    type Response = BranchSuggestionsResponse;
}

/// Paths on the daemon's machine for a typed query: the entries under an absolute prefix such
/// as `/root/dev/` (with `~` expanded), or a fuzzy match of a name under the home directory.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectorySuggestions {
    pub query: String,
    pub include_directories: bool,
    pub include_files: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectorySuggestionsResponse {
    #[serde(default)]
    pub directories: Vec<String>,
    pub error: Option<String>,
}

impl Request for DirectorySuggestions {
    const TYPE: &'static str = "directory_suggestions_request";
    const RESPONSE_TYPE: &'static str = "directory_suggestions_response";
    type Response = DirectorySuggestionsResponse;
}

/// Adds a directory as a project, without creating a workspace in it.
#[derive(Debug, Clone, Serialize)]
pub struct AddProject {
    pub cwd: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddProjectResponse {
    pub project: Option<ProjectSummary>,
    pub error: Option<String>,
}

impl Request for AddProject {
    const TYPE: &'static str = "project.add.request";
    const RESPONSE_TYPE: &'static str = "project.add.response";
    type Response = AddProjectResponse;
}

/// Removes a project from Paseo: archives its workspaces (their agents and terminals) and
/// forgets the project. Files on disk, worktrees included, stay.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveProject {
    pub project_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveProjectResponse {
    pub accepted: bool,
    #[serde(default)]
    pub removed_workspace_ids: Vec<String>,
    pub error: Option<String>,
}

impl Request for RemoveProject {
    const TYPE: &'static str = "project.remove.request";
    const RESPONSE_TYPE: &'static str = "project.remove.response";
    type Response = RemoveProjectResponse;
    // The daemon archives every workspace of the project first.
    const TIMEOUT: Option<Duration> = Some(Duration::from_secs(120));
}

/// The daemon machine's GitHub repositories matching `query`, through its `gh` CLI.
#[derive(Debug, Clone, Serialize)]
pub struct SearchGithubRepositories {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchGithubRepositoriesResponse {
    /// `success`, or why there are no results: `unavailable` (no `gh`), `unauthenticated` or
    /// `error`.
    pub status: String,
    #[serde(default)]
    pub repositories: Vec<GithubRepository>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubRepository {
    pub name_with_owner: String,
    pub description: Option<String>,
    pub visibility: String,
    pub clone_url: String,
}

impl Request for SearchGithubRepositories {
    const TYPE: &'static str = "workspace.github.search_repositories.request";
    const RESPONSE_TYPE: &'static str = "workspace.github.search_repositories.response";
    type Response = SearchGithubRepositoriesResponse;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CloneProtocol {
    Https,
    Ssh,
}

/// Clones a repository into `target_directory/<name>` on the daemon's machine and adds it as a
/// project. `repo` is a clone URL, or `owner/name` with a `clone_protocol`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneGithubProject {
    pub repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_protocol: Option<CloneProtocol>,
    pub target_directory: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneGithubProjectResponse {
    pub checkout_path: Option<String>,
    pub project: Option<ProjectSummary>,
    pub error: Option<String>,
}

impl Request for CloneGithubProject {
    const TYPE: &'static str = "project.github.clone.request";
    const RESPONSE_TYPE: &'static str = "project.github.clone.response";
    type Response = CloneGithubProjectResponse;
    // The daemon gives `git clone` five minutes.
    const TIMEOUT: Option<Duration> = Some(Duration::from_secs(330));
}

/// Closes agents and terminals the way closing their tabs in Paseo's apps does: agents are
/// archived, terminals killed.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseItems {
    pub agent_ids: Vec<String>,
    pub terminal_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseItemsResponse {
    #[serde(default)]
    pub agents: Vec<ClosedAgent>,
    #[serde(default)]
    pub terminals: Vec<ClosedTerminal>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClosedAgent {
    pub agent_id: String,
    pub archived_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClosedTerminal {
    pub terminal_id: String,
    pub success: bool,
}

impl Request for CloseItems {
    const TYPE: &'static str = "close_items_request";
    const RESPONSE_TYPE: &'static str = "close_items_response";
    type Response = CloseItemsResponse;
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureTerminal {
    pub terminal_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<i64>,
    pub strip_ansi: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureTerminalResponse {
    pub terminal_id: String,
    pub lines: Vec<String>,
    pub total_lines: u64,
}

impl Request for CaptureTerminal {
    const TYPE: &'static str = "capture_terminal_request";
    const RESPONSE_TYPE: &'static str = "capture_terminal_response";
    type Response = CaptureTerminalResponse;
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalStreamExit {
    pub subscription_id: Option<String>,
    pub terminal_id: String,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------------------------

/// Reads a file on the daemon's machine: `path` relative to `cwd`. Without `acceptBinary` the
/// daemon answers in the JSON response, a binary file base64-encoded.
#[derive(Debug, Clone, Serialize)]
pub struct ReadFile {
    pub cwd: String,
    pub path: String,
    /// Always "file"; "list" lists a directory.
    pub mode: &'static str,
}

impl ReadFile {
    pub fn new(cwd: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            cwd: cwd.into(),
            path: path.into(),
            mode: "file",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReadFileResponse {
    #[serde(default)]
    pub file: Option<ExplorerFile>,
    #[serde(default)]
    pub error: Option<String>,
}

impl Request for ReadFile {
    const TYPE: &'static str = "file_explorer_request";
    const RESPONSE_TYPE: &'static str = "file_explorer_response";
    type Response = ReadFileResponse;
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplorerFile {
    /// "text", "image" or "binary".
    pub kind: String,
    /// "utf-8", "base64" or "none".
    pub encoding: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------------------------

/// Lists the usage of the AI subscriptions the daemon's machine is logged into, one report per
/// account, as the Paseo app's Usage shows it. Needs the `usageSources` feature. The daemon caches
/// each report for five minutes; `force_refresh` skips the cache.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListUsageReports {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force_refresh: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListUsageReportsResponse {
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UsageReportUpdate {
    pub report: UsageReportEntry,
}

impl Request for ListUsageReports {
    const TYPE: &'static str = "usage.list_reports.request";
    const RESPONSE_TYPE: &'static str = "usage.list_reports.response";
    type Response = ListUsageReportsResponse;
}

impl StreamingRequest for ListUsageReports {
    const UPDATE_TYPE: &'static str = "usage.list_reports.update";
    type Update = UsageReportUpdate;
}

/// One account's usage, e.g. Claude or Codex.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReportEntry {
    pub id: String,
    #[serde(default)]
    pub account: UsageAccount,
    pub fetched_at: String,
    pub source_id: String,
    /// e.g. "Claude".
    pub source_label: String,
    /// The source's icon as SVG markup.
    #[serde(default)]
    pub icon: Option<String>,
    pub report: UsageReport,
    /// The last report that had data, kept while the current one is an error.
    #[serde(default)]
    pub last_successful: Option<LastSuccessfulUsage>,
}

impl UsageReportEntry {
    /// The report's data, or the last data it had while it can't be read.
    pub fn available(&self) -> Option<&AvailableUsage> {
        match &self.report {
            UsageReport::Available(report) => Some(report),
            _ => self.last_successful.as_ref().map(|last| &last.report),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct UsageAccount {
    /// e.g. the account's email.
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastSuccessfulUsage {
    pub fetched_at: String,
    pub report: AvailableUsage,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UsageReport {
    Available(AvailableUsage),
    Unavailable {
        problem: UsageProblem,
    },
    Error {
        error: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableUsage {
    /// e.g. "Max 5x".
    #[serde(default)]
    pub plan_label: Option<String>,
    #[serde(default)]
    pub windows: Vec<UsageWindow>,
    #[serde(default)]
    pub balances: Vec<UsageBalance>,
    #[serde(default)]
    pub details: Vec<UsageDetail>,
}

/// Why a report has no data.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UsageProblem {
    /// The login expired.
    Expired {},
    /// The provider refused the login.
    Rejected {},
    /// The account has no quota to report.
    NoQuota { detail: String },
    #[serde(other)]
    Unknown,
}

/// A rate limit window, e.g. the five-hour or weekly one.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    pub id: String,
    /// e.g. "Session" or "Weekly".
    pub label: String,
    /// e.g. "5h" or "wk": an empty one shows the percent alone, none shows `label`.
    #[serde(default)]
    pub short_label: Option<String>,
    /// Shown in the compact summary.
    #[serde(default)]
    pub summary: bool,
    #[serde(default)]
    pub used_pct: Option<f64>,
    #[serde(default)]
    pub remaining_pct: Option<f64>,
    #[serde(default)]
    pub resets_at: Option<String>,
    #[serde(default)]
    pub tone: UsageTone,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBalance {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub used: Option<f64>,
    #[serde(default)]
    pub remaining: Option<f64>,
    #[serde(default)]
    pub limit: Option<f64>,
    /// "usd", "credits", "requests" or "tokens".
    pub unit: String,
    #[serde(default)]
    pub tone: UsageTone,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct UsageDetail {
    pub id: String,
    pub label: String,
    pub value: String,
    #[serde(default)]
    pub tone: UsageTone,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageTone {
    Ok,
    Warning,
    Danger,
    /// Also "default".
    #[default]
    #[serde(other)]
    Default,
}

// ---------------------------------------------------------------------------------------------
// Pushed messages
// ---------------------------------------------------------------------------------------------

/// A session message the daemon pushed without it being the response to a pending request.
#[derive(Debug, Clone)]
pub enum ServerMessage {
    WorkspaceUpdate(WorkspaceUpdate),
    AgentUpdate(AgentUpdate),
    ProjectUpdate(ProjectUpdate),
    AgentAttentionRequired(AgentAttentionRequired),
    AgentPermissionRequested(AgentPermissionRequested),
    AgentPermissionResolved(AgentPermissionResolved),
    AgentStream(AgentStream),
    AgentTimelineReplacement(AgentTimelineReplacement),
    TerminalsChanged(TerminalsChanged),
    TerminalStreamExit(TerminalStreamExit),
    /// A `status` message other than `server_info`, for example `agent_created`.
    Status(Value),
    /// Any message type pazedo does not model yet, or one that failed to decode.
    Other {
        message_type: String,
        payload: Value,
    },
}

impl ServerMessage {
    pub(crate) fn decode(message_type: String, payload: Value) -> Self {
        fn decode_as<T: DeserializeOwned>(
            message_type: String,
            payload: Value,
            wrap: impl FnOnce(T) -> ServerMessage,
        ) -> ServerMessage {
            match serde_json::from_value::<T>(payload.clone()) {
                Ok(value) => wrap(value),
                Err(error) => {
                    log::warn!("failed to decode Paseo {message_type} message: {error}");
                    ServerMessage::Other {
                        message_type,
                        payload,
                    }
                }
            }
        }

        match message_type.as_str() {
            "workspace_update" => decode_as(message_type, payload, Self::WorkspaceUpdate),
            "agent_update" => decode_as(message_type, payload, Self::AgentUpdate),
            "project.update" => decode_as(message_type, payload, Self::ProjectUpdate),
            "agent_attention_required" => {
                decode_as(message_type, payload, Self::AgentAttentionRequired)
            }
            "agent_permission_request" => {
                decode_as(message_type, payload, Self::AgentPermissionRequested)
            }
            "agent_permission_resolved" => {
                decode_as(message_type, payload, Self::AgentPermissionResolved)
            }
            "agent_stream" => decode_as(message_type, payload, Self::AgentStream),
            "agent.timeline.replacement" => {
                decode_as(message_type, payload, Self::AgentTimelineReplacement)
            }
            "terminals_changed" => decode_as(message_type, payload, Self::TerminalsChanged),
            "terminal_stream_exit" => decode_as(message_type, payload, Self::TerminalStreamExit),
            "status" => Self::Status(payload),
            _ => Self::Other {
                message_type,
                payload,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_workspace_from_daemon_0_15_4() {
        // Captured from a 0.15.4 daemon's fetch_workspaces_response.
        let workspace: Workspace = serde_json::from_value(json!({
            "id": "wks_a83b0a48d685d33e",
            "projectId": "prj_4257c9872e53ebae",
            "projectDisplayName": "pazedo-sandbox",
            "projectCustomName": null,
            "projectCustomIconRevision": null,
            "projectRootPath": "/Users/me/dev/pazedo-sandbox",
            "workspaceDirectory": "/Users/me/dev/pazedo-sandbox",
            "setupStatus": null,
            "projectKind": "git",
            "workspaceKind": "local_checkout",
            "name": "main",
            "title": null,
            "pinnedAt": null,
            "background": false,
            "archivingAt": null,
            "status": "done",
            "statusEnteredAt": "2026-11-14T14:38:36.292Z",
            "activityAt": null,
            "createdAt": "2026-11-14T14:38:36.292Z",
            "diffStat": null,
            "scripts": [],
            "gitRuntime": {
                "currentBranch": "main",
                "remoteUrl": null,
                "isPaseoOwnedWorktree": false,
                "isDirty": false,
                "aheadBehind": null
            },
            "githubRuntime": {"featuresEnabled": false, "pullRequest": null, "error": null},
            "lastUserMessageAt": null
        }))
        .unwrap();
        assert_eq!(workspace.directory(), "/Users/me/dev/pazedo-sandbox");
        assert_eq!(workspace.status, WorkspaceStatus::Done);
        assert_eq!(workspace.workspace_kind, WorkspaceKind::LocalCheckout);
        assert_eq!(
            workspace.git_runtime.and_then(|git| git.current_branch),
            Some("main".to_string())
        );
    }

    #[test]
    fn encodes_workspace_creation_like_paseo_app() {
        // The shapes `workspace.create.request` accepted on a 0.15.4 daemon.
        let worktree = CreateWorkspace {
            source: WorkspaceSource::Worktree {
                cwd: "/repo".into(),
                project_id: Some("prj_1".into()),
                action: WorktreeAction::BranchOff,
                ref_name: Some("main".into()),
                branch_name: None,
            },
            title: None,
        };
        assert_eq!(
            serde_json::to_value(&worktree).unwrap(),
            json!({"source": {"kind": "worktree", "cwd": "/repo", "projectId": "prj_1",
                              "action": "branch-off", "refName": "main"}})
        );
        let local = CreateWorkspace {
            source: WorkspaceSource::Directory {
                path: "/repo".into(),
                project_id: None,
            },
            title: Some("Fix tests".into()),
        };
        assert_eq!(
            serde_json::to_value(&local).unwrap(),
            json!({"source": {"kind": "directory", "path": "/repo"}, "title": "Fix tests"})
        );
    }

    #[test]
    fn unknown_enum_values_do_not_fail_decoding() {
        let update: WorkspaceUpdate = serde_json::from_value(json!({
            "kind": "upsert",
            "workspace": {
                "id": "w", "projectId": "p", "projectDisplayName": "P",
                "projectRootPath": "/p", "projectKind": "something_new",
                "workspaceKind": "remote_checkout", "name": "n",
                "status": "sleeping", "activityAt": null, "brandNewField": 1
            }
        }))
        .unwrap();
        let WorkspaceUpdate::Upsert { workspace, .. } = update else {
            panic!("expected upsert");
        };
        assert_eq!(workspace.status, WorkspaceStatus::Unknown);
        assert_eq!(workspace.project_kind, ProjectKind::Unknown);
        assert_eq!(workspace.directory(), "/p");
    }

    #[test]
    fn decodes_timeline_items_and_stream_events() {
        // Shapes captured from a 0.15.4 daemon running the mock provider.
        let tool_call = TimelineItem::from_value(&json!({
            "type": "tool_call",
            "callId": "call-1",
            "name": "edit",
            "status": "completed",
            "error": null,
            "detail": {"type": "edit", "filePath": "src/main.rs", "unifiedDiff": "@@ -1 +1 @@"}
        }));
        let TimelineItem::ToolCall(tool_call) = tool_call else {
            panic!("expected a tool call");
        };
        assert_eq!(tool_call.status, ToolCallStatus::Completed);
        assert!(matches!(
            tool_call.detail(),
            ToolCallDetail::Edit { file_path, unified_diff: Some(_), .. } if file_path == "src/main.rs"
        ));

        assert_eq!(
            AgentStreamEvent::from_value(&json!({
                "type": "timeline",
                "provider": "mock",
                "turnId": "t1",
                "item": {"type": "assistant_message", "text": " Cycle 1"}
            })),
            AgentStreamEvent::Timeline {
                item: TimelineItem::AssistantMessage {
                    text: " Cycle 1".into()
                },
                turn_id: Some("t1".into()),
            }
        );
        assert!(matches!(
            AgentStreamEvent::from_value(&json!({
                "type": "permission_requested",
                "provider": "mock",
                "request": {"id": "p1", "provider": "mock", "name": "ExitPlanMode", "kind": "plan",
                            "actions": [{"id": "approve", "label": "Approve", "behavior": "allow"}]}
            })),
            AgentStreamEvent::PermissionRequested(request) if request.actions.len() == 1
        ));
        assert!(matches!(
            TimelineItem::from_value(&json!({"type": "compaction", "status": "completed"})),
            TimelineItem::Other(_)
        ));
        assert_eq!(
            serde_json::to_value(PermissionResponse::Allow {
                selected_action_id: Some("approve".into())
            })
            .unwrap(),
            json!({"behavior": "allow", "selectedActionId": "approve"})
        );
    }

    #[test]
    fn undecodable_known_message_falls_back_to_other() {
        let message = ServerMessage::decode("agent_update".into(), json!({"kind": "upsert"}));
        assert!(matches!(message, ServerMessage::Other { .. }));
    }
}
