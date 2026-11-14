//! Paseo agents as ACP sessions, so Zed's thread UI (messages, tool calls, plans, permission
//! prompts, the message editor) can show agents that a Paseo daemon owns.
//!
//! The daemon runs the agents; this connection only observes and steers them. It uses the
//! receipt submission transport: sending a message is acknowledged when the daemon accepts it,
//! and turn state is reported from the agent's timeline (`turn_started`, `turn_completed`, ...),
//! so turns started by other clients, the CLI or other agents show up the same way as turns
//! started here.

use acp_thread::{
    AcpThread, AgentConnection, MessageKind, PermissionRequestId, ReceiptSessionSubmissions,
    ReceiptSubmission, SubmissionDisconnected,
};
use action_log::ActionLog;
use agent_client_protocol::v2::schema as acp_v2;
use agent_servers::{AgentServer, AgentServerDelegate};
use anyhow::{Context as _, Result, anyhow, bail};
use collections::HashMap;
use gpui::{
    App, AppContext as _, Context, Entity, Global, SharedString, Subscription, Task, TaskExt as _,
    WeakEntity,
};
use paseo_client::{
    Client,
    protocol::{
        AgentSessionConfig, AgentStatus, AgentStream, AgentStreamEvent, CancelAgent, CreateAgent,
        FetchAgentTimeline, FetchAgentTimelineResponse, PermissionBehavior, PermissionRequest,
        PermissionResponse, ReleaseSubscription, RespondToPermission, SendAgentMessage,
        SetAgentTimelineSubscription, TimelineDirection, TimelineItem, TimelineProjection,
        TodoItem, ToolCall, ToolCallDetail, ToolCallStatus,
    },
};
use project::{AgentId, Project};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{any::Any, cell::RefCell, path::Path, rc::Rc};
use util::{ResultExt as _, path_list::PathList};

use crate::store::{PaseoStore, PaseoStoreEvent};

pub const PASEO_AGENT_ID: &str = "paseo";
/// Provider and model for agents created from Zed, until pazedo has a picker.
pub const PROVIDER_ENV_VAR: &str = "PAZEDO_AGENT_PROVIDER";
pub const MODEL_ENV_VAR: &str = "PAZEDO_AGENT_MODEL";
const DEFAULT_PROVIDER: &str = "claude";
const HISTORY_LIMIT: u32 = 200;
/// Long tool output is shortened in tool cards; the full output stays in Paseo.
const TOOL_OUTPUT_LIMIT: usize = 4000;

/// An in-process "agent server" for `ConversationView`: connecting returns the shared
/// [`PaseoConnection`].
pub struct PaseoAgentServer;

impl AgentServer for PaseoAgentServer {
    fn logo(&self) -> ui::IconName {
        ui::IconName::Sparkle
    }

    fn agent_id(&self) -> AgentId {
        AgentId::new(PASEO_AGENT_ID)
    }

    fn connect(
        &self,
        _delegate: AgentServerDelegate,
        _project: Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        Task::ready(Ok(PaseoConnection::global(cx) as Rc<dyn AgentConnection>))
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

struct GlobalPaseoConnection(Rc<PaseoConnection>);

impl Global for GlobalPaseoConnection {}

type Sessions = Rc<RefCell<HashMap<String, Session>>>;

pub struct PaseoConnection {
    store: Entity<PaseoStore>,
    sessions: Sessions,
    _store_subscription: Subscription,
}

/// One open agent: the thread showing it and the state needed to apply its live events.
struct Session {
    thread: WeakEntity<AcpThread>,
    stream: StreamState,
    /// While positive, live events are held instead of applied: during history loading, and
    /// between sending a message and the thread applying its acceptance. Paseo puts the user
    /// message on the timeline before answering the send, and the thread only recognises the
    /// echo of a submission it already knows was accepted.
    holds: usize,
    held: Vec<AgentStream>,
    last_seq: Option<u64>,
    timeline_subscription: Option<String>,
    /// Paseo permission request id → the prompt shown in the thread.
    permissions: HashMap<String, PermissionRequestId>,
    _release_subscription: Subscription,
}

impl PaseoConnection {
    pub fn global(cx: &mut App) -> Rc<Self> {
        if let Some(connection) = cx.try_global::<GlobalPaseoConnection>() {
            return connection.0.clone();
        }
        let store = PaseoStore::global(cx);
        let sessions = Sessions::default();
        let store_subscription = cx.subscribe(&store, {
            let sessions = sessions.clone();
            let store = store.downgrade();
            move |_, event: &PaseoStoreEvent, cx| {
                if let Some(store) = store.upgrade() {
                    handle_store_event(&sessions, &store, event, cx);
                }
            }
        });
        let connection = Rc::new(Self {
            store,
            sessions,
            _store_subscription: store_subscription,
        });
        cx.set_global(GlobalPaseoConnection(connection.clone()));
        connection
    }

    fn client(&self, cx: &App) -> Result<Client> {
        self.store
            .read(cx)
            .client()
            .filter(Client::is_connected)
            .context("Not connected to Paseo")
    }

    fn open_session(
        self: Rc<Self>,
        agent_id: String,
        project: Entity<Project>,
        work_dirs: PathList,
        title: Option<SharedString>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let existing = self
            .sessions
            .borrow()
            .get(&agent_id)
            .and_then(|session| session.thread.upgrade());
        if let Some(thread) = existing {
            return Task::ready(Ok(thread));
        }
        let client = match self.client(cx) {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        let title = title.or_else(|| {
            self.store
                .read(cx)
                .agent(&agent_id)
                .and_then(|agent| agent.title.clone())
                .map(Into::into)
        });

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let thread = cx.new(|cx| {
            AcpThread::new(
                None,
                title,
                Some(work_dirs),
                self.clone(),
                project,
                action_log,
                acp_v2::SessionId::new(agent_id.clone()),
                watch::Receiver::constant(acp_v2::PromptCapabilities::default()),
                cx,
            )
        });
        thread.update(cx, |thread, cx| thread.mark_activity_unconfirmed(cx));

        let release_subscription = cx.observe_release(&thread, {
            let sessions = self.sessions.clone();
            let store = self.store.downgrade();
            let agent_id = agent_id.clone();
            move |_, cx| {
                let removed = sessions.borrow_mut().remove(&agent_id);
                let subscription_id = removed.and_then(|session| session.timeline_subscription);
                if let (Some(subscription_id), Some(store)) = (subscription_id, store.upgrade())
                    && let Some(client) = store.read(cx).client()
                {
                    cx.background_spawn(async move {
                        client
                            .request(&ReleaseSubscription { subscription_id })
                            .await
                            .log_err();
                    })
                    .detach();
                }
            }
        });
        self.sessions.borrow_mut().insert(
            agent_id.clone(),
            Session {
                thread: thread.downgrade(),
                stream: StreamState::default(),
                holds: 1,
                held: Vec::new(),
                last_seq: None,
                timeline_subscription: None,
                permissions: HashMap::default(),
                _release_subscription: release_subscription,
            },
        );

        let sessions = self.sessions.clone();
        let store = self.store.clone();
        cx.spawn(async move |cx| {
            // Subscribe before fetching history so no event falls between the two; events
            // already covered by the history are dropped by sequence number.
            let loaded = async {
                let subscription = client
                    .request(&SetAgentTimelineSubscription {
                        agent_ids: vec![agent_id.clone()],
                    })
                    .await?;
                let history = client
                    .request(&FetchAgentTimeline {
                        agent_id: agent_id.clone(),
                        direction: TimelineDirection::Tail,
                        cursor: None,
                        limit: Some(HISTORY_LIMIT),
                        projection: TimelineProjection::Projected,
                        whole_turns: Some(true),
                    })
                    .await?;
                anyhow::Ok((subscription.subscription_id, history))
            }
            .await;
            cx.update(|cx| match loaded {
                Ok((subscription_id, history)) => {
                    if let Some(session) = sessions.borrow_mut().get_mut(&agent_id) {
                        session.timeline_subscription = subscription_id;
                    }
                    replay_history(&sessions, &store, &agent_id, &thread, history, cx);
                    Ok(thread)
                }
                Err(error) => {
                    sessions.borrow_mut().remove(&agent_id);
                    Err(error.context("Failed to load the Paseo agent"))
                }
            })
        })
    }
}

impl AgentConnection for PaseoConnection {
    fn agent_id(&self) -> AgentId {
        AgentId::new(PASEO_AGENT_ID)
    }

    fn telemetry_id(&self) -> SharedString {
        PASEO_AGENT_ID.into()
    }

    /// Creates a Paseo agent in the workspace's directory. It starts idle; the first message
    /// starts its first turn.
    fn new_session(
        self: Rc<Self>,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let client = match self.client(cx) {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        let Some(cwd) = work_dirs
            .paths()
            .first()
            .map(|path| path.to_string_lossy().to_string())
        else {
            return Task::ready(Err(anyhow!("Open a folder to start a Paseo agent in it")));
        };
        let workspace_id = self
            .store
            .read(cx)
            .newest_workspace_for_directory(Path::new(&cwd))
            .map(|workspace| workspace.id.clone());
        let request = CreateAgent {
            config: AgentSessionConfig {
                provider: std::env::var(PROVIDER_ENV_VAR)
                    .unwrap_or_else(|_| DEFAULT_PROVIDER.to_string()),
                cwd,
                model: std::env::var(MODEL_ENV_VAR).ok(),
                ..Default::default()
            },
            workspace_id,
            initial_prompt: None,
        };
        cx.spawn(async move |cx| {
            let response = client.request(&request).await?;
            let agent = match (response.agent, response.error) {
                (Some(agent), _) => agent,
                (None, Some(error)) => bail!("Paseo could not create the agent: {error}"),
                (None, None) => bail!("Paseo did not return the new agent"),
            };
            cx.update(|cx| self.open_session(agent.id, project, work_dirs, None, cx))
                .await
        })
    }

    fn supports_load_session(&self) -> bool {
        true
    }

    fn load_session(
        self: Rc<Self>,
        session_id: acp_v2::SessionId,
        project: Entity<Project>,
        work_dirs: PathList,
        title: Option<SharedString>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        self.open_session(session_id.0.to_string(), project, work_dirs, title, cx)
    }

    /// Paseo agents outlive this window; closing the view only stops watching.
    fn should_close_sessions_on_release(&self) -> bool {
        false
    }

    /// Paseo resolves pending permissions itself when a turn is cancelled.
    fn answers_permission_requests_on_cancel(&self) -> bool {
        false
    }

    fn auth_methods(&self) -> &[acp_v2::AuthMethod] {
        &[]
    }

    fn authenticate(&self, _method: acp_v2::AuthMethodId, _cx: &mut App) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn receipt_submissions(
        &self,
        session_id: &acp_v2::SessionId,
        _cx: &App,
    ) -> Option<Rc<dyn ReceiptSessionSubmissions>> {
        Some(Rc::new(PaseoSubmissions {
            agent_id: session_id.0.to_string(),
            store: self.store.downgrade(),
            sessions: self.sessions.clone(),
        }))
    }

    fn cancel(&self, session_id: &acp_v2::SessionId, cx: &mut App) -> Result<()> {
        let client = self.client(cx)?;
        let agent_id = session_id.0.to_string();
        cx.background_spawn(async move {
            let response = client.request(&CancelAgent { agent_id }).await?;
            if let Some(error) = response.error {
                bail!("Paseo could not stop the agent: {error}");
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
        Ok(())
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

struct PaseoSubmissions {
    agent_id: String,
    store: WeakEntity<PaseoStore>,
    sessions: Sessions,
}

impl ReceiptSessionSubmissions for PaseoSubmissions {
    fn submit(
        &self,
        content: Vec<acp_v2::ContentBlock>,
        cx: &mut Context<AcpThread>,
    ) -> ReceiptSubmission {
        let client = self
            .store
            .upgrade()
            .and_then(|store| store.read(cx).client());
        let agent_id = self.agent_id.clone();
        // Paseo echoes this id as the user message's `messageId`, which is how the thread
        // matches the echo to this submission.
        let message_id = uuid::Uuid::new_v4().to_string();
        let text = prompt_text(&content);

        if let Some(session) = self.sessions.borrow_mut().get_mut(&agent_id) {
            session.holds += 1;
        }
        let (applied_tx, applied_rx) = futures::channel::oneshot::channel::<bool>();
        cx.spawn({
            let sessions = self.sessions.clone();
            let store = self.store.clone();
            let agent_id = agent_id.clone();
            async move |_, cx| {
                // Resolves when the thread has applied the outcome, or is dropped if it never will.
                applied_rx.await.ok();
                cx.update(|cx| {
                    if let Some(store) = store.upgrade() {
                        release_hold(&sessions, &store, &agent_id, cx);
                    }
                });
            }
        })
        .detach();

        let response = cx.background_spawn(async move {
            let client = client.context("Not connected to Paseo")?;
            let response = client
                .request(&SendAgentMessage {
                    agent_id,
                    text,
                    message_id: Some(message_id.clone()),
                    active_turn_behavior: None,
                })
                .await
                .map_err(|error| {
                    if client.is_connected() {
                        error
                    } else {
                        error.context(SubmissionDisconnected)
                    }
                })?;
            if !response.accepted {
                bail!(
                    "Paseo did not accept the message: {}",
                    response.error.unwrap_or_else(|| "no reason given".into())
                );
            }
            Ok(acp_v2::PromptResponse::new(message_id))
        });
        ReceiptSubmission::ordered(Box::pin(response), applied_tx)
    }
}

/// Ends one hold on the session's live events. When the last hold ends, held events are applied
/// one per update cycle: the thread view handles each new entry by index when the cycle ends,
/// so adding several entries in one cycle desynchronises it. The hold stays until the queue is
/// drained, so events arriving meanwhile are applied after the held ones.
fn release_hold(sessions: &Sessions, store: &Entity<PaseoStore>, agent_id: &str, cx: &mut App) {
    {
        let mut sessions = sessions.borrow_mut();
        let Some(session) = sessions.get_mut(agent_id) else {
            return;
        };
        if session.holds > 1 {
            session.holds -= 1;
            return;
        }
    }
    let sessions = sessions.clone();
    let store = store.downgrade();
    let agent_id = agent_id.to_string();
    cx.spawn(async move |cx| {
        loop {
            let drained = cx.update(|cx| {
                let next = {
                    let mut sessions = sessions.borrow_mut();
                    let Some(session) = sessions.get_mut(&agent_id) else {
                        return true;
                    };
                    if session.held.is_empty() {
                        session.holds = session.holds.saturating_sub(1);
                        return true;
                    }
                    session.held.remove(0)
                };
                if let Some(store) = store.upgrade() {
                    apply_stream(&sessions, &store, &next, cx);
                }
                false
            });
            if drained {
                break;
            }
        }
    })
    .detach();
}

/// Paseo takes plain text; mentions and other blocks are sent as their textual form.
fn prompt_text(content: &[acp_v2::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| {
            let value = serde_json::to_value(block).ok()?;
            match value.get("type").and_then(Value::as_str) {
                Some("text") => value
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                Some("resource_link") => {
                    value.get("uri").and_then(Value::as_str).map(str::to_string)
                }
                Some("resource") => value
                    .get("resource")
                    .and_then(|resource| resource.get("uri"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                _ => None,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn handle_store_event(
    sessions: &Sessions,
    store: &Entity<PaseoStore>,
    event: &PaseoStoreEvent,
    cx: &mut App,
) {
    match event {
        PaseoStoreEvent::AgentStream(stream) => {
            let ready = {
                let mut sessions = sessions.borrow_mut();
                let Some(session) = sessions.get_mut(&stream.agent_id) else {
                    return;
                };
                if session.holds > 0 {
                    session.held.push(stream.clone());
                    false
                } else {
                    true
                }
            };
            if ready {
                apply_stream(sessions, store, stream, cx);
            }
        }
        // Subscriptions do not survive a reconnect and replaced history cannot be patched,
        // so the view shows Zed's "connection lost" state with its reconnect button, which
        // loads the agent again.
        PaseoStoreEvent::Disconnected => {
            let threads: Vec<_> = sessions
                .borrow_mut()
                .drain()
                .filter_map(|(_, session)| session.thread.upgrade())
                .collect();
            for thread in threads {
                thread.update(cx, |thread, cx| thread.handle_connection_lost(cx));
            }
        }
        PaseoStoreEvent::AgentTimelineReplacement(replacement) => {
            let thread = sessions
                .borrow_mut()
                .remove(&replacement.agent_id)
                .and_then(|session| session.thread.upgrade());
            if let Some(thread) = thread {
                thread.update(cx, |thread, cx| thread.handle_connection_lost(cx));
            }
        }
        PaseoStoreEvent::Connected => {}
    }
}

fn replay_history(
    sessions: &Sessions,
    store: &Entity<PaseoStore>,
    agent_id: &str,
    thread: &Entity<AcpThread>,
    history: FetchAgentTimelineResponse,
    cx: &mut App,
) {
    let mut updates = Vec::new();
    let mut last_key = None;
    for entry in &history.entries {
        let key = format!("history:{}", entry.seq_start);
        last_key = match entry.item() {
            TimelineItem::AssistantMessage { .. } => Some((MessageKind::Assistant, key.clone())),
            TimelineItem::Reasoning { .. } => Some((MessageKind::Thought, key.clone())),
            _ => None,
        };
        updates.extend(history_updates(&entry.item(), &key));
    }

    let agent = history
        .agent
        .or_else(|| store.read(cx).agent(agent_id).cloned());
    let running = agent.as_ref().is_some_and(|agent| {
        matches!(
            agent.status,
            AgentStatus::Running | AgentStatus::Initializing
        )
    });
    let pending_permissions = agent
        .as_ref()
        .map(|agent| agent.pending_permissions.clone())
        .unwrap_or_default();
    updates.push(ThreadUpdate::State(if !pending_permissions.is_empty() {
        state_requires_action()
    } else if running {
        state_running()
    } else {
        state_idle("end_turn")
    }));
    apply_updates(thread, updates, cx);

    {
        let mut sessions = sessions.borrow_mut();
        let Some(session) = sessions.get_mut(agent_id) else {
            return;
        };
        session.last_seq = history.end_cursor.as_ref().map(|cursor| cursor.seq);
        session.stream.turn_active = running || !pending_permissions.is_empty();
        // A turn still running continues its last message instead of starting a new one.
        if running && let Some((kind, key)) = last_key {
            match kind {
                MessageKind::Thought => session.stream.thought = Some(key),
                _ => session.stream.assistant = Some(key),
            }
        }
    }
    for request in pending_permissions {
        raise_permission(sessions, store, agent_id, thread, request, cx);
    }
    release_hold(sessions, store, agent_id, cx);
}

fn apply_stream(
    sessions: &Sessions,
    store: &Entity<PaseoStore>,
    stream: &AgentStream,
    cx: &mut App,
) {
    let event = stream.event();
    let (thread, updates) = {
        let mut sessions = sessions.borrow_mut();
        let Some(session) = sessions.get_mut(&stream.agent_id) else {
            return;
        };
        if let Some(seq) = stream.seq {
            if session.last_seq.is_some_and(|last_seq| seq <= last_seq) {
                return;
            }
            if let Some(last_seq) = session.last_seq
                && seq > last_seq + 1
            {
                log::warn!(
                    "Paseo agent {}: timeline gap between {last_seq} and {seq}",
                    stream.agent_id
                );
            }
            session.last_seq = Some(seq);
        }
        let Some(thread) = session.thread.upgrade() else {
            return;
        };
        (thread, session.stream.updates(&event))
    };
    apply_updates(&thread, updates, cx);

    match event {
        AgentStreamEvent::PermissionRequested(request) => {
            raise_permission(sessions, store, &stream.agent_id, &thread, request, cx);
        }
        AgentStreamEvent::PermissionResolved { request_id, .. } => {
            // Answered elsewhere (another client, the CLI, a timeout): withdraw the prompt.
            let local = sessions
                .borrow_mut()
                .get_mut(&stream.agent_id)
                .and_then(|session| session.permissions.remove(&request_id));
            if let Some(local) = local {
                thread.update(cx, |thread, cx| thread.cancel_permission_request(local, cx));
            }
        }
        _ => {}
    }
}

fn raise_permission(
    sessions: &Sessions,
    store: &Entity<PaseoStore>,
    agent_id: &str,
    thread: &Entity<AcpThread>,
    request: PermissionRequest,
    cx: &mut App,
) {
    let already_shown = sessions
        .borrow()
        .get(agent_id)
        .is_some_and(|session| session.permissions.contains_key(&request.id));
    if already_shown {
        return;
    }
    let (wire_request, actions) = permission_request(agent_id, &request);
    let raised = thread.update(cx, |thread, cx| thread.request_permission(wire_request, cx));
    let (local_id, outcome) = match raised {
        Ok(raised) => raised,
        Err(error) => {
            log::error!(
                "could not show Paseo permission request {}: {error:#}",
                request.id
            );
            return;
        }
    };
    if let Some(session) = sessions.borrow_mut().get_mut(agent_id) {
        session.permissions.insert(request.id.clone(), local_id);
    }

    let sessions = sessions.clone();
    let store = store.downgrade();
    let agent_id = agent_id.to_string();
    cx.spawn(async move |cx| {
        let outcome = outcome.await;
        let still_ours = sessions
            .borrow_mut()
            .get_mut(&agent_id)
            .and_then(|session| session.permissions.remove(&request.id))
            .is_some();
        let acp_v2::RequestPermissionOutcome::Selected(selected) = outcome else {
            return anyhow::Ok(());
        };
        if !still_ours {
            return Ok(());
        }
        let option_id = selected.option_id.0.to_string();
        let response = permission_response(&actions, &option_id);
        let client = cx
            .update(|cx| store.upgrade().and_then(|store| store.read(cx).client()))
            .context("Not connected to Paseo")?;
        client.notify(&RespondToPermission {
            agent_id,
            request_id: request.id,
            response,
        })
    })
    .detach_and_log_err(cx);
}

/// The ACP prompt for a Paseo permission request, and the options it offers.
fn permission_request(
    agent_id: &str,
    request: &PermissionRequest,
) -> (
    acp_v2::RequestPermissionRequest,
    Vec<(String, PermissionBehavior)>,
) {
    let actions: Vec<(String, String, PermissionBehavior)> = if request.actions.is_empty() {
        vec![
            ("allow".into(), "Allow".into(), PermissionBehavior::Allow),
            ("deny".into(), "Deny".into(), PermissionBehavior::Deny),
        ]
    } else {
        request
            .actions
            .iter()
            .map(|action| (action.id.clone(), action.label.clone(), action.behavior))
            .collect()
    };
    let options: Vec<Value> = actions
        .iter()
        .map(|(id, label, behavior)| {
            json!({
                "optionId": id,
                "name": label,
                "kind": match behavior {
                    PermissionBehavior::Allow => "allow_once",
                    PermissionBehavior::Deny => "reject_once",
                },
            })
        })
        .collect();
    let title = request
        .title
        .clone()
        .unwrap_or_else(|| format!("{} wants to run {}", request.provider, request.name));
    let wire = wire(json!({
        "sessionId": agent_id,
        "title": title,
        "description": request.description,
        "options": options,
    }));
    let actions = actions
        .into_iter()
        .map(|(id, _, behavior)| (id, behavior))
        .collect();
    (wire, actions)
}

fn permission_response(
    actions: &[(String, PermissionBehavior)],
    option_id: &str,
) -> PermissionResponse {
    match actions.iter().find(|(id, _)| id == option_id) {
        Some((_, PermissionBehavior::Allow)) => PermissionResponse::Allow {
            selected_action_id: Some(option_id.to_string()),
        },
        _ => PermissionResponse::Deny {
            selected_action_id: Some(option_id.to_string()),
            message: None,
            interrupt: None,
        },
    }
}

/// What to do to a thread, decoupled from gpui so the translation can be unit tested.
#[derive(Debug)]
enum ThreadUpdate {
    User(acp_v2::UserMessage),
    Assistant(acp_v2::AgentMessage),
    Thought(acp_v2::AgentThought),
    Chunk(MessageKind, acp_v2::ContentChunk),
    ToolCall(acp_v2::ToolCallUpdate),
    Plan(acp_v2::PlanItems),
    State(acp_v2::StateUpdate),
}

fn apply_updates(thread: &Entity<AcpThread>, updates: Vec<ThreadUpdate>, cx: &mut App) {
    thread.update(cx, |thread, cx| {
        for update in updates {
            let result = match update {
                ThreadUpdate::User(message) => thread.upsert_user_message(message, cx),
                ThreadUpdate::Assistant(message) => thread.upsert_assistant_message(message, cx),
                ThreadUpdate::Thought(thought) => thread.upsert_thought(thought, cx),
                ThreadUpdate::Chunk(kind, chunk) => thread.append_message_chunk(kind, chunk, cx),
                ThreadUpdate::ToolCall(tool_call) => thread.upsert_wire_tool_call(tool_call, cx),
                ThreadUpdate::Plan(plan) => {
                    thread.upsert_plan_items(plan, None, cx);
                    Ok(())
                }
                ThreadUpdate::State(state) => thread.update_session_state(state, cx),
            };
            if let Err(error) = result {
                log::warn!("could not apply a Paseo update: {error:#}");
            }
        }
    });
}

/// Which live message the next text delta extends. Paseo streams assistant text and reasoning
/// as deltas without message ids; a delta continues the current message until something else
/// (a tool call, a user message, a new turn) happens in between.
#[derive(Default)]
struct StreamState {
    turn: Option<String>,
    /// Paseo can announce `turn_completed` before `permission_resolved`, so resolving a
    /// permission only means "running again" while a turn is still active.
    turn_active: bool,
    assistant: Option<String>,
    thought: Option<String>,
    next_message: u64,
}

impl StreamState {
    fn new_key(&mut self, kind: &str) -> String {
        self.next_message += 1;
        format!(
            "live:{}:{kind}:{}",
            self.turn.as_deref().unwrap_or("none"),
            self.next_message
        )
    }

    fn updates(&mut self, event: &AgentStreamEvent) -> Vec<ThreadUpdate> {
        match event {
            AgentStreamEvent::TurnStarted { turn_id } => {
                self.turn = turn_id.clone();
                self.turn_active = true;
                self.assistant = None;
                self.thought = None;
                vec![ThreadUpdate::State(state_running())]
            }
            AgentStreamEvent::TurnCompleted { .. } => {
                self.turn_active = false;
                vec![ThreadUpdate::State(state_idle("end_turn"))]
            }
            AgentStreamEvent::TurnCanceled { .. } => {
                self.turn_active = false;
                vec![ThreadUpdate::State(state_idle("cancelled"))]
            }
            AgentStreamEvent::TurnFailed { error, .. } => {
                self.turn_active = false;
                self.assistant = None;
                let key = self.new_key("error");
                vec![
                    ThreadUpdate::Assistant(wire(json!({
                        "messageId": key,
                        "content": [text_block(&format!("**The turn failed:** {error}"))],
                    }))),
                    ThreadUpdate::State(state_idle("end_turn")),
                ]
            }
            AgentStreamEvent::Timeline { item, .. } => self.timeline_updates(item),
            AgentStreamEvent::PermissionRequested(_) => {
                vec![ThreadUpdate::State(state_requires_action())]
            }
            AgentStreamEvent::PermissionResolved { .. } if self.turn_active => {
                vec![ThreadUpdate::State(state_running())]
            }
            AgentStreamEvent::PermissionResolved { .. } => Vec::new(),
            AgentStreamEvent::AttentionRequired { .. } | AgentStreamEvent::Other(_) => Vec::new(),
        }
    }

    fn timeline_updates(&mut self, item: &TimelineItem) -> Vec<ThreadUpdate> {
        match item {
            TimelineItem::AssistantMessage { text } => {
                self.thought = None;
                let key = match &self.assistant {
                    Some(key) => key.clone(),
                    None => {
                        let key = self.new_key("assistant");
                        self.assistant = Some(key.clone());
                        key
                    }
                };
                vec![ThreadUpdate::Chunk(
                    MessageKind::Assistant,
                    chunk(&key, text),
                )]
            }
            TimelineItem::Reasoning { text } => {
                self.assistant = None;
                let key = match &self.thought {
                    Some(key) => key.clone(),
                    None => {
                        let key = self.new_key("thought");
                        self.thought = Some(key.clone());
                        key
                    }
                };
                vec![ThreadUpdate::Chunk(MessageKind::Thought, chunk(&key, text))]
            }
            other => {
                self.assistant = None;
                self.thought = None;
                let key = self.new_key("item");
                history_updates(other, &key)
            }
        }
    }
}

/// Updates for one complete timeline item. `key` identifies it when the item has no id.
fn history_updates(item: &TimelineItem, key: &str) -> Vec<ThreadUpdate> {
    match item {
        TimelineItem::UserMessage { text, message_id } => vec![ThreadUpdate::User(wire(json!({
            "messageId": message_id.as_deref().unwrap_or(key),
            "content": [text_block(text)],
        })))],
        TimelineItem::AssistantMessage { text } => {
            vec![ThreadUpdate::Assistant(wire(json!({
                "messageId": key,
                "content": [text_block(text)],
            })))]
        }
        TimelineItem::Reasoning { text } => vec![ThreadUpdate::Thought(wire(json!({
            "messageId": key,
            "content": [text_block(text)],
        })))],
        TimelineItem::ToolCall(tool_call) => {
            vec![ThreadUpdate::ToolCall(tool_call_update(tool_call))]
        }
        TimelineItem::Todo { items } => vec![ThreadUpdate::Plan(plan(items))],
        TimelineItem::Error { message } => vec![ThreadUpdate::Assistant(wire(json!({
            "messageId": key,
            "content": [text_block(&format!("**Error:** {message}"))],
        })))],
        TimelineItem::Notification { level, message } => {
            vec![ThreadUpdate::Assistant(wire(json!({
                "messageId": key,
                "content": [text_block(&format!("**{level}:** {message}"))],
            })))]
        }
        TimelineItem::Other(_) => Vec::new(),
    }
}

fn tool_call_update(tool_call: &ToolCall) -> acp_v2::ToolCallUpdate {
    let detail = tool_call.detail();
    let (title, kind, body) = match &detail {
        ToolCallDetail::Shell {
            command,
            output,
            exit_code,
            ..
        } => (
            // Zed renders an execute tool call's title as the command, in code style.
            command.clone(),
            "execute",
            output.as_deref().map(|output| {
                let mut body = format!("```\n{}\n```", truncate(output));
                if let Some(exit_code) = exit_code.filter(|code| *code != 0) {
                    body.push_str(&format!("\nExit code {exit_code}"));
                }
                body
            }),
        ),
        ToolCallDetail::Read { file_path, .. } => (format!("Read {file_path}"), "read", None),
        ToolCallDetail::Edit {
            file_path,
            unified_diff,
            ..
        } => (
            format!("Edit {file_path}"),
            "edit",
            unified_diff
                .as_deref()
                .map(|diff| format!("```diff\n{}\n```", truncate(diff))),
        ),
        ToolCallDetail::Write { file_path, .. } => (format!("Write {file_path}"), "edit", None),
        ToolCallDetail::Search { query, content } => (
            format!("Search {query}"),
            "search",
            content
                .as_deref()
                .map(|content| truncate(content).to_string()),
        ),
        ToolCallDetail::Fetch { url, result } => (
            format!("Fetch {url}"),
            "fetch",
            result.as_deref().map(|result| truncate(result).to_string()),
        ),
        ToolCallDetail::SubAgent { description, .. } => (
            description.clone().unwrap_or_else(|| "Sub-agent".into()),
            "other",
            None,
        ),
        ToolCallDetail::PlainText { label, text } => (
            label.clone().unwrap_or_else(|| tool_call.name.clone()),
            "other",
            text.clone(),
        ),
        ToolCallDetail::Plan { text } => ("Plan".to_string(), "think", Some(text.clone())),
        ToolCallDetail::Other(_) => (tool_call.name.clone(), "other", None),
    };
    let error = tool_call.error.as_ref().map(|error| match error {
        Value::String(error) => error.clone(),
        error => error.to_string(),
    });
    let body = match (body, error) {
        (Some(body), Some(error)) => Some(format!("{body}\n\n**Error:** {error}")),
        (None, Some(error)) => Some(format!("**Error:** {error}")),
        (body, None) => body,
    };
    let status = match tool_call.status {
        ToolCallStatus::Running => "in_progress",
        ToolCallStatus::Completed => "completed",
        ToolCallStatus::Failed => "failed",
        ToolCallStatus::Canceled => "cancelled",
        ToolCallStatus::Unknown => "pending",
    };
    let mut update = json!({
        "toolCallId": tool_call.call_id,
        "name": tool_call.name,
        "title": title,
        "kind": kind,
        "status": status,
    });
    if let Some(body) = body {
        update["content"] = json!([{"type": "content", "content": text_block(&body)}]);
    }
    wire(update)
}

fn plan(items: &[TodoItem]) -> acp_v2::PlanItems {
    let entries: Vec<Value> = items
        .iter()
        .map(|item| {
            let status = match item.status.as_deref() {
                _ if item.completed => "completed",
                Some("in_progress") => "in_progress",
                _ => "pending",
            };
            json!({"content": item.text, "priority": "medium", "status": status})
        })
        .collect();
    wire(json!({"planId": "paseo-todo", "entries": entries}))
}

fn truncate(text: &str) -> &str {
    match text.char_indices().nth(TOOL_OUTPUT_LIMIT) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn chunk(key: &str, text: &str) -> acp_v2::ContentChunk {
    wire(json!({"messageId": key, "content": text_block(text)}))
}

fn state_running() -> acp_v2::StateUpdate {
    wire(json!({"state": "running"}))
}

fn state_requires_action() -> acp_v2::StateUpdate {
    wire(json!({"state": "requires_action"}))
}

fn state_idle(stop_reason: &str) -> acp_v2::StateUpdate {
    wire(json!({"state": "idle", "stopReason": stop_reason}))
}

/// Builds ACP values from their wire form. The schema types are non-exhaustive builders whose
/// constructors change between releases; the wire format is the stable contract. A shape this
/// module gets wrong is a bug caught by the tests below, hence the panic.
fn wire<T: DeserializeOwned>(value: Value) -> T {
    match serde_json::from_value(value.clone()) {
        Ok(parsed) => parsed,
        Err(error) => panic!(
            "invalid ACP {}: {error}: {value}",
            std::any::type_name::<T>()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paseo_client::protocol::PermissionKind;
    use pretty_assertions::assert_eq;

    fn timeline(item: Value) -> AgentStreamEvent {
        AgentStreamEvent::from_value(&json!({"type": "timeline", "provider": "mock", "item": item}))
    }

    fn keys(updates: &[ThreadUpdate]) -> Vec<String> {
        updates
            .iter()
            .map(|update| match update {
                ThreadUpdate::User(message) => format!("user {}", message.message_id.0),
                ThreadUpdate::Assistant(message) => format!("assistant {}", message.message_id.0),
                ThreadUpdate::Thought(message) => format!("thought {}", message.message_id.0),
                ThreadUpdate::Chunk(kind, chunk) => {
                    format!("{kind:?} chunk {}", chunk.message_id.0)
                }
                ThreadUpdate::ToolCall(tool_call) => format!("tool {}", tool_call.tool_call_id.0),
                ThreadUpdate::Plan(plan) => format!("plan {}", plan.entries.len()),
                ThreadUpdate::State(state) => format!(
                    "state {}",
                    serde_json::to_value(state).unwrap()["state"]
                        .as_str()
                        .unwrap()
                ),
            })
            .collect()
    }

    #[test]
    fn deltas_extend_one_message_until_something_else_happens() {
        let mut state = StreamState::default();
        let mut updates = Vec::new();
        for event in [
            AgentStreamEvent::from_value(
                &json!({"type": "turn_started", "provider": "mock", "turnId": "t1"}),
            ),
            timeline(json!({"type": "reasoning", "text": "Need to"})),
            timeline(json!({"type": "reasoning", "text": " look"})),
            timeline(json!({"type": "assistant_message", "text": "## Cycle"})),
            timeline(json!({"type": "assistant_message", "text": " 1"})),
            timeline(
                json!({"type": "tool_call", "callId": "c1", "name": "read", "status": "running",
                            "error": null, "detail": {"type": "read", "filePath": "src/main.rs"}}),
            ),
            timeline(json!({"type": "assistant_message", "text": "Done."})),
            timeline(json!({"type": "user_message", "text": "thanks", "messageId": "m-9"})),
            AgentStreamEvent::from_value(
                &json!({"type": "turn_completed", "provider": "mock", "turnId": "t1"}),
            ),
        ] {
            updates.extend(state.updates(&event));
        }
        assert_eq!(
            keys(&updates),
            vec![
                "state running",
                "Thought chunk live:t1:thought:1",
                "Thought chunk live:t1:thought:1",
                "Assistant chunk live:t1:assistant:2",
                "Assistant chunk live:t1:assistant:2",
                "tool c1",
                "Assistant chunk live:t1:assistant:4",
                "user m-9",
                "state idle",
            ]
        );
    }

    #[test]
    fn a_permission_resolved_after_the_turn_ended_keeps_the_thread_idle() {
        let mut state = StreamState::default();
        let event = |value: Value| AgentStreamEvent::from_value(&value);
        let mut updates = Vec::new();
        for value in [
            json!({"type": "turn_started", "provider": "mock", "turnId": "t1"}),
            json!({"type": "permission_requested", "provider": "mock",
                   "request": {"id": "p1", "provider": "mock", "name": "plan", "kind": "plan"}}),
            json!({"type": "turn_completed", "provider": "mock", "turnId": "t1"}),
            json!({"type": "permission_resolved", "provider": "mock", "requestId": "p1",
                   "resolution": {"behavior": "allow"}}),
        ] {
            updates.extend(state.updates(&event(value)));
        }
        assert_eq!(
            keys(&updates),
            vec!["state running", "state requires_action", "state idle"]
        );
    }

    #[test]
    fn translates_tool_calls() {
        let tool_call = |detail: Value, status: &str, error: Value| -> Value {
            let ThreadUpdate::ToolCall(update) = history_updates(
                &TimelineItem::from_value(&json!({
                    "type": "tool_call", "callId": "c", "name": "bash",
                    "status": status, "error": error, "detail": detail,
                })),
                "k",
            )
            .remove(0) else {
                panic!("expected a tool call");
            };
            serde_json::to_value(update).unwrap()
        };

        let shell = tool_call(
            json!({"type": "shell", "command": "cargo test", "output": "ok", "exitCode": 1}),
            "failed",
            json!("tests failed"),
        );
        assert_eq!(shell["title"], "cargo test");
        assert_eq!(shell["kind"], "execute");
        assert_eq!(shell["status"], "failed");
        assert_eq!(
            shell["content"][0]["content"]["text"],
            "```\nok\n```\nExit code 1\n\n**Error:** tests failed"
        );

        let edit = tool_call(
            json!({"type": "edit", "filePath": "src/main.rs", "unifiedDiff": "@@ -1 +1 @@\n-a\n+b"}),
            "completed",
            Value::Null,
        );
        assert_eq!(edit["title"], "Edit src/main.rs");
        assert_eq!(edit["kind"], "edit");
        assert_eq!(edit["status"], "completed");

        let unknown = tool_call(json!({"type": "brand_new"}), "running", Value::Null);
        assert_eq!(unknown["title"], "bash");
        assert_eq!(unknown["status"], "in_progress");
    }

    #[test]
    fn builds_state_plan_and_permission_values() {
        assert_eq!(
            serde_json::to_value(state_idle("cancelled")).unwrap(),
            json!({"state": "idle", "stopReason": "cancelled"})
        );
        assert_eq!(
            serde_json::to_value(state_requires_action()).unwrap()["state"],
            "requires_action"
        );
        let plan = plan(&[
            serde_json::from_value(json!({"text": "a", "completed": true})).unwrap(),
            serde_json::from_value(
                json!({"text": "b", "completed": false, "status": "in_progress"}),
            )
            .unwrap(),
        ]);
        assert_eq!(
            serde_json::to_value(&plan.entries).unwrap(),
            json!([
                {"content": "a", "priority": "medium", "status": "completed"},
                {"content": "b", "priority": "medium", "status": "in_progress"},
            ])
        );

        let request: PermissionRequest = serde_json::from_value(json!({
            "id": "mock-plan-1", "provider": "mock", "name": "ExitPlanMode", "kind": "plan",
            "title": "Plan",
            "actions": [
                {"id": "implement", "label": "Implement", "behavior": "allow"},
                {"id": "dismiss", "label": "Dismiss", "behavior": "deny"},
            ],
        }))
        .unwrap();
        assert_eq!(request.kind, PermissionKind::Plan);
        let (wire_request, actions) = permission_request("agent-1", &request);
        assert_eq!(wire_request.title, "Plan");
        assert_eq!(wire_request.options.len(), 2);
        assert_eq!(
            permission_response(&actions, "dismiss"),
            PermissionResponse::Deny {
                selected_action_id: Some("dismiss".into()),
                message: None,
                interrupt: None,
            }
        );
        assert_eq!(
            permission_response(&actions, "implement"),
            PermissionResponse::Allow {
                selected_action_id: Some("implement".into())
            }
        );
    }

    #[test]
    fn prompt_text_keeps_text_and_links() {
        let content: Vec<acp_v2::ContentBlock> = wire(json!([
            {"type": "text", "text": "Fix"},
            {"type": "resource_link", "uri": "file:///src/main.rs", "name": "main.rs"},
        ]));
        assert_eq!(prompt_text(&content), "Fix\nfile:///src/main.rs");
    }
}
