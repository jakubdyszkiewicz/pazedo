//! Paseo agents as ACP sessions, so Zed's thread UI (messages, tool calls, plans, permission
//! prompts, the message editor) can show agents that a Paseo daemon owns.
//!
//! The daemon runs the agents; this connection only observes and steers them. It uses the
//! receipt submission transport: sending a message is acknowledged when the daemon accepts it,
//! and turn state is reported from the agent's timeline (`turn_started`, `turn_completed`, ...),
//! so turns started by other clients, the CLI or other agents show up the same way as turns
//! started here.

use acp_thread::{
    AcpThread, AgentConnection, AgentSessionConfigOptions, ElicitationEntryId, MessageKind,
    PermissionRequestId, ReceiptSessionSubmissions, ReceiptSubmission, SubmissionDisconnected,
};
use action_log::ActionLog;
use agent_client_protocol::{schema::MaybeUndefined, v2::schema as acp_v2};
use agent_servers::{AgentServer, AgentServerDelegate};
use agent_settings::AgentSettings;
use anyhow::{Context as _, Result, anyhow, bail};
use collections::{HashMap, HashSet};
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, Global, SharedString, Subscription, Task,
    TaskExt as _, WeakEntity,
};
use paseo_client::{
    Client,
    protocol::{
        AgentMessage, AgentMessageEvent, AgentSessionConfig, AgentSlashCommand, AgentStatus,
        AgentStream, AgentStreamEvent, CancelAgent, CreateAgent, FetchAgentTimeline,
        FetchAgentTimelineResponse, ImageAttachment, ListCommands, PermissionBehavior,
        PermissionRequest, PermissionResponse, Question, ReleaseSubscription, RespondToPermission,
        SendAgentMessage, SetAgentMode, SetAgentModel, SetAgentThinking,
        SetAgentTimelineSubscription, TimelineDirection, TimelineItem, TimelineProjection,
        TodoItem, ToolCall, ToolCallDetail, ToolCallStatus,
    },
};
use project::{AgentId, Project};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use settings::Settings as _;
use std::{
    any::Any,
    cell::RefCell,
    mem,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};
use util::{ResultExt as _, path_list::PathList};

use crate::agent_images;
use crate::store::{PaseoStore, PaseoStoreEvent};

pub const PASEO_AGENT_ID: &str = "paseo";
/// Provider and model for agents created from Zed, until pazedo has a picker.
pub const PROVIDER_ENV_VAR: &str = "PAZEDO_AGENT_PROVIDER";
pub const MODEL_ENV_VAR: &str = "PAZEDO_AGENT_MODEL";
const DEFAULT_PROVIDER: &str = "claude";
/// Rows, not entries: every streamed chunk is a row, so a few turns can take hundreds. Paseo's
/// app pages 40 at a time as you scroll back; Zed's thread view cannot page backwards, so this
/// loads enough to cover recent work and the tab offers the rest.
const HISTORY_LIMIT: u32 = 2000;
/// Long tool output is shortened in tool cards; the full output stays in Paseo.
const TOOL_OUTPUT_LIMIT: usize = 4000;
/// How long a thread stays running after its turn ends, in case another turn follows. Paseo
/// ends a turn and starts the next when a message interrupts a running agent, and Claude ends
/// its turn before taking a steered message or waking up after background work. Going idle in
/// between would show Zed's "agent finished" notification while the agent is still working.
const TURN_END_GRACE: Duration = Duration::from_secs(1);

/// How many timeline entries a session loads at first. `PAZEDO_AGENT_HISTORY_LIMIT` overrides
/// it, which makes truncated history easy to try.
fn history_limit() -> u32 {
    std::env::var("PAZEDO_AGENT_HISTORY_LIMIT")
        .ok()
        .and_then(|limit| limit.parse().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(HISTORY_LIMIT)
}

/// An in-process "agent server" for `ConversationView`: connecting returns the
/// [`PaseoConnection`] of the agent's daemon.
pub struct PaseoAgentServer {
    pub store: Entity<PaseoStore>,
}

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
        Task::ready(Ok(
            PaseoConnection::for_store(&self.store, cx) as Rc<dyn AgentConnection>
        ))
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

/// One connection per daemon, by its store's entity id.
#[derive(Default)]
struct GlobalPaseoConnections(HashMap<EntityId, Rc<PaseoConnection>>);

impl Global for GlobalPaseoConnections {}

type Sessions = Rc<RefCell<HashMap<String, Session>>>;

pub struct PaseoConnection {
    store: Entity<PaseoStore>,
    sessions: Sessions,
    /// Agents whose next load fetches their whole history instead of the latest entries.
    full_history: RefCell<HashSet<String>>,
    /// Provider and model for the next `new_session`. `ConversationView` creates the session
    /// itself, so the choice made before opening the tab is handed over here.
    next_new_agent: RefCell<Option<NewAgent>>,
    _store_subscription: Subscription,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewAgent {
    pub provider: String,
    pub model: Option<String>,
}

impl NewAgent {
    /// `PAZEDO_AGENT_PROVIDER` / `PAZEDO_AGENT_MODEL`, or Claude with its default model.
    pub fn from_env() -> Self {
        Self {
            provider: std::env::var(PROVIDER_ENV_VAR)
                .unwrap_or_else(|_| DEFAULT_PROVIDER.to_string()),
            model: std::env::var(MODEL_ENV_VAR).ok(),
        }
    }
}

/// One open agent: the thread showing it and the state needed to apply its live events.
struct Session {
    thread: WeakEntity<AcpThread>,
    /// Whether the loaded history stops short of the agent's first entry.
    history_truncated: bool,
    full_history: bool,
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
    permissions: HashMap<String, ShownPermission>,
    /// Counts changes to the thread's running state, so that an ended turn going idle after
    /// `TURN_END_GRACE` knows whether something happened since.
    state_generation: u64,
    /// Whether the user stopped the agent from this thread, whose cancellation goes idle at
    /// once: no turn follows it.
    stopping: bool,
    /// The images of messages sent from this thread, by message id. Paseo's echo of a message
    /// has only its text, and the echo replaces the message in the thread.
    sent_images: HashMap<String, Vec<acp_v2::ContentBlock>>,
    _release_subscription: Subscription,
}

impl PaseoConnection {
    pub(crate) fn store(&self) -> &Entity<PaseoStore> {
        &self.store
    }

    /// The connection of `store`'s daemon, shared by all of its agents' threads.
    pub fn for_store(store: &Entity<PaseoStore>, cx: &mut App) -> Rc<Self> {
        if let Some(connection) = cx
            .try_global::<GlobalPaseoConnections>()
            .and_then(|connections| connections.0.get(&store.entity_id()))
        {
            return connection.clone();
        }
        let store = store.clone();
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
            full_history: RefCell::default(),
            next_new_agent: RefCell::default(),
            _store_subscription: store_subscription,
        });
        cx.default_global::<GlobalPaseoConnections>()
            .0
            .insert(connection.store.entity_id(), connection.clone());
        connection
    }

    pub fn set_next_new_agent(&self, new_agent: NewAgent) {
        *self.next_new_agent.borrow_mut() = Some(new_agent);
    }

    /// Whether the open session shows only the latest part of the agent's history.
    pub fn history_truncated(&self, agent_id: &str) -> bool {
        self.sessions
            .borrow()
            .get(agent_id)
            .is_some_and(|session| session.history_truncated)
    }

    /// Makes the agent's next load fetch its whole history. The caller reopens the view.
    pub fn request_full_history(&self, agent_id: &str) {
        self.full_history.borrow_mut().insert(agent_id.to_string());
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
        let full_history = self.full_history.borrow().contains(&agent_id);
        let existing = self
            .sessions
            .borrow()
            .get(&agent_id)
            .filter(|session| session.full_history || !full_history)
            .and_then(|session| session.thread.upgrade());
        if let Some(thread) = existing {
            return Task::ready(Ok(thread));
        }
        let client = match self.client(cx) {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        let agent = self.store.read(cx).agent(&agent_id).cloned();
        let title = title.or_else(|| {
            agent
                .as_ref()
                .and_then(|agent| agent.title.clone())
                .map(Into::into)
        });
        let cwd = agent
            .map(|agent| PathBuf::from(agent.cwd))
            .or_else(|| work_dirs.paths().first().map(|path| path.to_path_buf()));

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
                // Paseo takes images with a message, for every provider; the message box only
                // accepts pasted or attached images when the agent advertises them.
                watch::Receiver::constant(
                    acp_v2::PromptCapabilities::new().image(acp_v2::PromptImageCapabilities::new()),
                ),
                cx,
            )
        });
        thread.update(cx, |thread, cx| thread.mark_activity_unconfirmed(cx));

        let release_subscription = cx.observe_release(&thread, {
            let sessions = self.sessions.clone();
            let store = self.store.downgrade();
            let agent_id = agent_id.clone();
            let thread_id = thread.entity_id();
            move |_, cx| {
                // A reload may already have replaced this session with a newer one.
                let removed = {
                    let mut sessions = sessions.borrow_mut();
                    let is_current = sessions
                        .get(&agent_id)
                        .is_some_and(|session| session.thread.entity_id() == thread_id);
                    if is_current {
                        sessions.remove(&agent_id)
                    } else {
                        None
                    }
                };
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
                history_truncated: false,
                full_history,
                stream: StreamState {
                    cwd,
                    ..StreamState::default()
                },
                holds: 1,
                held: Vec::new(),
                last_seq: None,
                timeline_subscription: None,
                permissions: HashMap::default(),
                state_generation: 0,
                stopping: false,
                sent_images: HashMap::default(),
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
                        // 0 means every entry.
                        limit: Some(if full_history { 0 } else { history_limit() }),
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
                        session.history_truncated = history.has_older;
                    }
                    replay_history(&sessions, &store, &agent_id, &thread, history, cx);
                    load_commands(client.clone(), agent_id.clone(), thread.downgrade(), cx);
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
            .workspace_for_directory(Path::new(&cwd))
            .map(|workspace| workspace.id.clone());
        let new_agent = self
            .next_new_agent
            .borrow_mut()
            .take()
            .unwrap_or_else(NewAgent::from_env);
        let request = CreateAgent {
            config: AgentSessionConfig {
                provider: new_agent.provider,
                cwd,
                model: new_agent.model,
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

    fn session_config_options(
        &self,
        session_id: &acp_v2::SessionId,
        cx: &App,
    ) -> Option<Rc<dyn AgentSessionConfigOptions>> {
        let options = PaseoConfigOptions {
            agent_id: session_id.0.to_string(),
            store: self.store.downgrade(),
            options: RefCell::default(),
        };
        *options.options.borrow_mut() = options.compute(None, cx);
        Some(Rc::new(options))
    }

    fn cancel(&self, session_id: &acp_v2::SessionId, cx: &mut App) -> Result<()> {
        let client = self.client(cx)?;
        let agent_id = session_id.0.to_string();
        if let Some(session) = self.sessions.borrow_mut().get_mut(&agent_id) {
            session.stopping = true;
        }
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

const MODEL_OPTION: &str = "model";
const MODE_OPTION: &str = "mode";
const THINKING_OPTION: &str = "thinking";

/// The agent's model, thinking level and mode as ACP config options, so Zed's own dropdowns
/// change them.
/// The provider cannot change after creation, so it is not offered.
struct PaseoConfigOptions {
    agent_id: String,
    store: WeakEntity<PaseoStore>,
    /// `config_options` gets no `App` to read the store with, so the options are computed when
    /// the view asks for them and whenever a value is set.
    options: RefCell<Vec<acp_v2::SessionConfigOption>>,
}

impl PaseoConfigOptions {
    /// The options for the agent as the store knows it, with `change` applied first so a set
    /// shows its result (and a new model its own thinking levels) before the daemon confirms.
    fn compute(&self, change: Option<(&str, &str)>, cx: &App) -> Vec<acp_v2::SessionConfigOption> {
        let Some(store) = self.store.upgrade() else {
            return Vec::new();
        };
        let store = store.read(cx);
        let Some(agent) = store.agent(&self.agent_id) else {
            return Vec::new();
        };
        let mut agent = agent.clone();
        match change {
            Some((MODEL_OPTION, value)) => {
                agent.model = Some(value.to_string());
                // The new model's default thinking level applies until one is picked.
                agent.thinking_option_id = None;
                agent.effective_thinking_option_id = None;
            }
            Some((MODE_OPTION, value)) => agent.current_mode_id = Some(value.to_string()),
            Some((THINKING_OPTION, value)) => {
                agent.thinking_option_id = Some(value.to_string());
                agent.effective_thinking_option_id = Some(value.to_string());
            }
            _ => {}
        }
        config_options(&agent, store.provider(&agent.provider))
    }
}

impl AgentSessionConfigOptions for PaseoConfigOptions {
    fn config_options(&self) -> Vec<acp_v2::SessionConfigOption> {
        self.options.borrow().clone()
    }

    fn set_config_option(
        &self,
        config_id: acp_v2::SessionConfigId,
        value: acp_v2::SessionConfigOptionValue,
        cx: &mut App,
    ) -> Task<Result<Vec<acp_v2::SessionConfigOption>>> {
        let client = self
            .store
            .upgrade()
            .and_then(|store| store.read(cx).client());
        let agent_id = self.agent_id.clone();
        let Some(value) = serde_json::to_value(&value).ok().and_then(|value| {
            value
                .get("value")
                .and_then(Value::as_str)
                .map(str::to_string)
        }) else {
            return Task::ready(Err(anyhow!("Unsupported value for {}", config_id.0)));
        };
        let config_id = config_id.0.to_string();
        let options = self.compute(Some((config_id.as_str(), value.as_str())), cx);
        *self.options.borrow_mut() = options.clone();
        cx.background_spawn(async move {
            let client = client.context("Not connected to Paseo")?;
            let response = match config_id.as_str() {
                MODEL_OPTION => {
                    client
                        .request(&SetAgentModel {
                            agent_id,
                            model_id: Some(value),
                        })
                        .await?
                }
                MODE_OPTION => {
                    client
                        .request(&SetAgentMode {
                            agent_id,
                            mode_id: value,
                        })
                        .await?
                }
                THINKING_OPTION => {
                    client
                        .request(&SetAgentThinking {
                            agent_id,
                            thinking_option_id: Some(value),
                        })
                        .await?
                }
                other => bail!("Paseo agents have no {other} option"),
            };
            if !response.accepted {
                bail!(
                    "Paseo did not apply the change: {}",
                    response.error.unwrap_or_else(|| "no reason given".into())
                );
            }
            Ok(options)
        })
    }
}

/// Model, thinking level and mode selectors for an agent. The provider's models and their
/// thinking levels come from the daemon's provider snapshot; the agent's snapshot lists its
/// modes.
fn config_options(
    agent: &paseo_client::protocol::Agent,
    provider: Option<&paseo_client::protocol::ProviderEntry>,
) -> Vec<acp_v2::SessionConfigOption> {
    let mut options = Vec::new();

    let mut models: Vec<(String, String, Option<String>)> = provider
        .map(|provider| {
            provider
                .models
                .iter()
                .filter(|model| model.is_selectable != Some(false))
                .map(|model| {
                    (
                        model.id.clone(),
                        model.label.clone(),
                        model.description.clone(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let current_model = agent.model.clone().or_else(|| {
        provider
            .and_then(|provider| provider.default_model())
            .map(|model| model.id.clone())
    });
    if let Some(current_model) = current_model {
        if !models.iter().any(|(id, _, _)| *id == current_model) {
            models.insert(0, (current_model.clone(), current_model.clone(), None));
        }
        options.push(wire(json!({
            "configId": MODEL_OPTION,
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": current_model,
            "options": models
                .into_iter()
                .map(|(id, label, description)| {
                    json!({"value": id, "name": label, "description": description})
                })
                .collect::<Vec<_>>(),
        })));
    }

    let model = provider.and_then(|provider| options_model(provider, agent.model.as_deref()));
    if let Some(model) = model
        && let Some(first) = model.thinking_options.first()
    {
        let current = agent
            .effective_thinking_option_id
            .clone()
            .or_else(|| agent.thinking_option_id.clone())
            .or_else(|| model.default_thinking_option_id.clone())
            .or_else(|| {
                model
                    .thinking_options
                    .iter()
                    .find(|option| option.is_default)
                    .map(|option| option.id.clone())
            })
            .unwrap_or_else(|| first.id.clone());
        options.push(wire(json!({
            "configId": THINKING_OPTION,
            "name": "Thinking",
            "category": "thought_level",
            "type": "select",
            "currentValue": current,
            "options": model
                .thinking_options
                .iter()
                .map(|option| {
                    json!({"value": option.id, "name": option.label, "description": option.description})
                })
                .collect::<Vec<_>>(),
        })));
    }

    if let Some(current_mode) = &agent.current_mode_id
        && !agent.available_modes.is_empty()
    {
        options.push(wire(json!({
            "configId": MODE_OPTION,
            "name": "Mode",
            "category": "mode",
            "type": "select",
            "currentValue": current_mode,
            "options": agent
                .available_modes
                .iter()
                .map(|mode| json!({"value": mode.id, "name": mode.label, "description": mode.description}))
                .collect::<Vec<_>>(),
        })));
    }
    options
}

/// The provider's model an agent runs: the one it names, else the provider's default.
fn options_model<'a>(
    provider: &'a paseo_client::protocol::ProviderEntry,
    model: Option<&str>,
) -> Option<&'a paseo_client::protocol::ProviderModel> {
    match model {
        Some(id) => provider.models.iter().find(|model| model.id == id),
        None => provider.default_model(),
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
        let images = prompt_images(&content);

        if let Some(session) = self.sessions.borrow_mut().get_mut(&agent_id) {
            session.holds += 1;
            let images: Vec<_> = content
                .iter()
                .filter(|block| matches!(block, acp_v2::ContentBlock::Image(_)))
                .cloned()
                .collect();
            if !images.is_empty() {
                session.sent_images.insert(message_id.clone(), images);
            }
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
                    images,
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

/// Puts the images of messages sent from this thread back into Paseo's echoes of them.
fn restore_sent_images(
    updates: &mut [ThreadUpdate],
    sent_images: &HashMap<String, Vec<acp_v2::ContentBlock>>,
) {
    for update in updates {
        if let ThreadUpdate::User(message) = update
            && let Some(images) = sent_images.get(&message.message_id.to_string())
            && let MaybeUndefined::Value(content) = &mut message.content
        {
            content.extend(images.iter().cloned());
        }
    }
}

/// The prompt's images, which Paseo takes alongside the text.
fn prompt_images(content: &[acp_v2::ContentBlock]) -> Vec<ImageAttachment> {
    content
        .iter()
        .filter_map(|block| match block {
            acp_v2::ContentBlock::Image(image) => Some(ImageAttachment {
                data: image.data.clone(),
                mime_type: image.mime_type.to_string(),
            }),
            _ => None,
        })
        .collect()
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
                    log::debug!(
                        "holding Paseo event seq={:?} (holds={})",
                        stream.seq,
                        session.holds
                    );
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
        PaseoStoreEvent::Connected
        | PaseoStoreEvent::TerminalStreamExit(_)
        | PaseoStoreEvent::UsageChanged => {}
    }
}

/// Offers the agent's slash commands and skills in the message box's `/` completion.
fn load_commands(client: Client, agent_id: String, thread: WeakEntity<AcpThread>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let response = client
            .request(&ListCommands {
                agent_id: agent_id.clone(),
            })
            .await?;
        if let Some(error) = response.error {
            bail!("Paseo could not list the commands of agent {agent_id}: {error}");
        }
        let commands = response
            .commands
            .into_iter()
            .map(available_command)
            .collect();
        thread.update(cx, |thread, cx| {
            thread.update_available_commands(commands, cx)
        })?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// Skills too are plain commands: Paseo runs both from the message's text.
fn available_command(command: AgentSlashCommand) -> acp_v2::AvailableCommand {
    let mut command_value = json!({
        "type": "command",
        "name": command.name,
        "description": command.description,
    });
    if !command.argument_hint.is_empty() {
        command_value["input"] = json!({"type": "text", "hint": command.argument_hint});
    }
    wire(command_value)
}

fn replay_history(
    sessions: &Sessions,
    store: &Entity<PaseoStore>,
    agent_id: &str,
    thread: &Entity<AcpThread>,
    history: FetchAgentTimelineResponse,
    cx: &mut App,
) {
    let cwd = sessions
        .borrow()
        .get(agent_id)
        .and_then(|session| session.stream.cwd.clone());
    let collapse_running_commands = !AgentSettings::get_global(cx).expand_terminal_card;
    let mut updates = Vec::new();
    let mut last_key = None;
    let cache_dir = agent_images::cache_dir();
    let mut images = Vec::new();
    for entry in &history.entries {
        let key = format!("history:{}", entry.seq_start);
        let item = agent_images::localize_item(entry.item(), &cache_dir, &mut images);
        last_key = match item {
            TimelineItem::AssistantMessage { .. } => Some((MessageKind::Assistant, key.clone())),
            TimelineItem::Reasoning { .. } => Some((MessageKind::Thought, key.clone())),
            _ => None,
        };
        updates.extend(history_updates(
            &item,
            &key,
            cwd.as_deref(),
            collapse_running_commands,
        ));
    }
    agent_images::fetch_images(store, images, cx);
    if let Some(session) = sessions.borrow().get(agent_id) {
        restore_sent_images(&mut updates, &session.sent_images);
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
        session.state_generation += 1;
        // The turn that is running now may have started while no events were applied.
        session.stream.turn = None;
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
    let mut images = Vec::new();
    let event =
        agent_images::localize_event(stream.event(), &agent_images::cache_dir(), &mut images);
    log::debug!(
        "Paseo agent {} event seq={:?} {:?}",
        stream.agent_id,
        stream.seq,
        stream.event_type()
    );
    let (thread, updates, deferred_end) = {
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
        session.stream.collapse_running_commands =
            !AgentSettings::get_global(cx).expand_terminal_card;
        let mut updates = session.stream.updates(&event);
        restore_sent_images(&mut updates, &session.sent_images);
        log::debug!("  -> {updates:?}");
        let end_turn = updates
            .iter()
            .position(|update| matches!(update, ThreadUpdate::EndTurn(_)))
            .map(|index| updates.remove(index));
        if end_turn.is_some()
            || updates
                .iter()
                .any(|update| matches!(update, ThreadUpdate::State(_)))
        {
            session.state_generation += 1;
        }
        let stopped = end_turn.is_some() && mem::take(&mut session.stopping);
        let deferred_end = match end_turn {
            Some(end_turn) if stopped && matches!(event, AgentStreamEvent::TurnCanceled { .. }) => {
                updates.push(end_turn);
                None
            }
            Some(end_turn) => Some((end_turn, session.state_generation)),
            None => None,
        };
        (thread, updates, deferred_end)
    };
    apply_updates(&thread, updates, cx);
    agent_images::fetch_images(store, images, cx);
    if let Some((end_turn, generation)) = deferred_end {
        let sessions = sessions.clone();
        let agent_id = stream.agent_id.clone();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(TURN_END_GRACE).await;
            cx.update(|cx| {
                let thread = sessions
                    .borrow()
                    .get(&agent_id)
                    .filter(|session| session.state_generation == generation)
                    .and_then(|session| session.thread.upgrade());
                if let Some(thread) = thread {
                    apply_updates(&thread, vec![end_turn], cx);
                }
            });
        })
        .detach();
    }

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
            match local {
                Some(ShownPermission::Prompt(local)) => {
                    thread.update(cx, |thread, cx| thread.cancel_permission_request(local, cx));
                }
                Some(ShownPermission::Form(local)) => {
                    thread.update(cx, |thread, cx| thread.cancel_elicitation(&local, cx));
                }
                None => {}
            }
        }
        _ => {}
    }
}

/// How a Paseo permission request is shown: questions are forms, the rest are prompts.
enum ShownPermission {
    Prompt(PermissionRequestId),
    Form(ElicitationEntryId),
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
    if let Some(questions) = request.questions() {
        raise_questions(
            sessions, store, agent_id, thread, request, questions, None, cx,
        );
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
        session
            .permissions
            .insert(request.id.clone(), ShownPermission::Prompt(local_id));
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

/// Shows a question request as a form, the way Paseo's app does: each question's options as
/// radio buttons or checkboxes, and a text field for an answer of one's own. `previous` holds
/// the values of a submitted form that left a question unanswered, to ask again with them.
#[allow(clippy::too_many_arguments)]
fn raise_questions(
    sessions: &Sessions,
    store: &Entity<PaseoStore>,
    agent_id: &str,
    thread: &Entity<AcpThread>,
    request: PermissionRequest,
    questions: Vec<Question>,
    previous: Option<&QuestionFormContent>,
    cx: &mut App,
) {
    let form = question_form(agent_id, &questions, previous);
    let raised = thread.update(cx, |thread, cx| {
        thread.request_elicitation_with_id(form, cx)
    });
    let (local_id, response) = match raised {
        Ok(raised) => raised,
        Err(error) => {
            log::error!(
                "could not show Paseo question {}: {}",
                request.id,
                error.message
            );
            return;
        }
    };
    if let Some(session) = sessions.borrow_mut().get_mut(agent_id) {
        session
            .permissions
            .insert(request.id.clone(), ShownPermission::Form(local_id));
    }

    let sessions = sessions.clone();
    let store = store.downgrade();
    let thread = thread.downgrade();
    let agent_id = agent_id.to_string();
    cx.spawn(async move |cx| {
        let response = response.await;
        let still_ours = sessions
            .borrow_mut()
            .get_mut(&agent_id)
            .and_then(|session| session.permissions.remove(&request.id))
            .is_some();
        if !still_ours {
            return anyhow::Ok(());
        }
        let response = match response.action {
            acp_v2::ElicitationAction::Accept(accept) => {
                let content = accept.content.unwrap_or_default();
                let Some(answers) = question_answers(&questions, &content) else {
                    cx.update(|cx| {
                        if let (Some(store), Some(thread)) = (store.upgrade(), thread.upgrade()) {
                            raise_questions(
                                &sessions,
                                &store,
                                &agent_id,
                                &thread,
                                request,
                                questions,
                                Some(&content),
                                cx,
                            );
                        }
                    });
                    return Ok(());
                };
                let mut updated_input = match request.input.clone() {
                    Some(Value::Object(input)) => input,
                    _ => serde_json::Map::new(),
                };
                updated_input.insert("answers".into(), Value::Object(answers));
                PermissionResponse::Allow {
                    selected_action_id: None,
                    updated_input: Some(Value::Object(updated_input)),
                }
            }
            _ => PermissionResponse::Deny {
                selected_action_id: None,
                message: Some("Dismissed by user".into()),
                interrupt: None,
            },
        };
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

type QuestionFormContent = std::collections::BTreeMap<String, acp_v2::ElicitationContentValue>;

fn question_choice_field(index: usize) -> String {
    format!("question_{index:02}")
}

fn question_text_field(index: usize) -> String {
    format!("question_{index:02}_text")
}

/// The form for a question request. Zed orders a form's fields by name, so the names number the
/// questions. Options are never required, because Zed preselects the first option of a required
/// choice; whether each question is answered is checked on submit instead.
fn question_form(
    agent_id: &str,
    questions: &[Question],
    previous: Option<&QuestionFormContent>,
) -> acp_v2::CreateElicitationRequest {
    let previous_value = |field: &str| previous.and_then(|previous| previous.get(field));
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (index, question) in questions.iter().enumerate() {
        if !question.options.is_empty() {
            let field = question_choice_field(index);
            let options: Vec<Value> = question
                .options
                .iter()
                .map(|option| {
                    json!({
                        "const": option.label,
                        "title": option.label,
                        "description": option.description,
                    })
                })
                .collect();
            let mut property = if question.multi_select {
                json!({
                    "type": "array",
                    "title": question.question,
                    "items": {"anyOf": options},
                })
            } else {
                json!({
                    "type": "string",
                    "title": question.question,
                    "oneOf": options,
                })
            };
            let is_option =
                |label: &str| question.options.iter().any(|option| option.label == label);
            match previous_value(&field) {
                Some(acp_v2::ElicitationContentValue::String(label)) if is_option(label) => {
                    property["default"] = json!(label);
                }
                Some(acp_v2::ElicitationContentValue::StringArray(labels))
                    if question.multi_select && labels.iter().all(|label| is_option(label)) =>
                {
                    property["default"] = json!(labels);
                }
                _ => {}
            }
            properties.insert(field, property);
        }
        if question.accepts_text() {
            let field = question_text_field(index);
            let title = if question.options.is_empty() {
                question.question.as_str()
            } else {
                "Other"
            };
            let mut property = json!({"type": "string", "title": title});
            if let Some(acp_v2::ElicitationContentValue::String(text)) = previous_value(&field) {
                property["default"] = json!(text);
            }
            if question.options.is_empty() && !question.allow_empty {
                required.push(field.clone());
            }
            properties.insert(field, property);
        }
    }
    let headers: Vec<&str> = questions
        .iter()
        .map(|question| question.header.as_str())
        .collect();
    let mut schema = json!({
        "type": "object",
        "properties": properties,
        "required": required,
    });
    if previous.is_some() {
        schema["description"] = json!("Answer every question.");
    }
    wire(json!({
        "mode": "form",
        "sessionId": agent_id,
        "message": headers.join(" · "),
        "requestedSchema": schema,
    }))
}

/// The answers of a submitted question form, keyed by header, or `None` when a question is left
/// unanswered. A typed answer replaces a single choice and joins multiple ones, as in Paseo's
/// app.
fn question_answers(
    questions: &[Question],
    content: &QuestionFormContent,
) -> Option<serde_json::Map<String, Value>> {
    let mut answers = serde_json::Map::new();
    for (index, question) in questions.iter().enumerate() {
        let mut parts: Vec<String> = match content.get(&question_choice_field(index)) {
            Some(acp_v2::ElicitationContentValue::String(label)) => vec![label.clone()],
            Some(acp_v2::ElicitationContentValue::StringArray(labels)) => labels.clone(),
            _ => Vec::new(),
        };
        let text = match content.get(&question_text_field(index)) {
            Some(acp_v2::ElicitationContentValue::String(text)) => text.trim(),
            _ => "",
        };
        if question.accepts_text() && !text.is_empty() {
            if !question.multi_select {
                parts.clear();
            }
            parts.push(text.to_string());
        }
        let answer = if !parts.is_empty() {
            parts.join(", ")
        } else if question.allow_empty && question.options.is_empty() {
            String::new()
        } else {
            return None;
        };
        answers.insert(question.header.clone(), Value::String(answer));
    }
    Some(answers)
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
            updated_input: None,
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
    /// The turn ended with this stop reason: the thread goes idle, after `TURN_END_GRACE` when
    /// it comes from the live stream.
    EndTurn(&'static str),
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
                ThreadUpdate::EndTurn(stop_reason) => {
                    thread.update_session_state(state_idle(stop_reason), cx)
                }
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
    /// The agent's directory, which relative tool-call paths are resolved against.
    cwd: Option<PathBuf>,
    /// See `history_updates`.
    collapse_running_commands: bool,
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
            AgentStreamEvent::TurnCompleted { turn_id } => self.end_turn(turn_id, "end_turn"),
            AgentStreamEvent::TurnCanceled { turn_id, .. } => self.end_turn(turn_id, "cancelled"),
            AgentStreamEvent::TurnFailed { turn_id, error } => {
                self.assistant = None;
                let key = self.new_key("error");
                let mut updates = vec![ThreadUpdate::Assistant(wire(json!({
                    "messageId": key,
                    "content": [text_block(&format!("**The turn failed:** {error}"))],
                })))];
                updates.extend(self.end_turn(turn_id, "end_turn"));
                updates
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

    /// Paseo still forwards the end of a turn that a newer turn replaced; that end leaves the
    /// newer turn running.
    fn end_turn(
        &mut self,
        turn_id: &Option<String>,
        stop_reason: &'static str,
    ) -> Vec<ThreadUpdate> {
        let replaced =
            self.turn_active && turn_id.is_some() && self.turn.is_some() && *turn_id != self.turn;
        if replaced {
            return Vec::new();
        }
        self.turn_active = false;
        vec![ThreadUpdate::EndTurn(stop_reason)]
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
                history_updates(
                    other,
                    &key,
                    self.cwd.as_deref(),
                    self.collapse_running_commands,
                )
            }
        }
    }
}

/// Updates for one complete timeline item. `key` identifies it when the item has no id.
/// `collapse_running_commands` (from `agent.expand_terminal_card` being off) shows running shell
/// commands as collapsed rows too, rather than Zed's command card.
fn history_updates(
    item: &TimelineItem,
    key: &str,
    cwd: Option<&Path>,
    collapse_running_commands: bool,
) -> Vec<ThreadUpdate> {
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
        TimelineItem::ToolCall(ToolCall {
            call_id,
            agent_message:
                Some(AgentMessage {
                    event: AgentMessageEvent::Message,
                    sender,
                    text,
                }),
            ..
        }) => vec![ThreadUpdate::User(wire(json!({
            "messageId": call_id,
            "content": [text_block(&format!(
                "From agent “{}”:\n\n{text}",
                sender.title.as_deref().unwrap_or(&sender.id)
            ))],
        })))],
        TimelineItem::ToolCall(tool_call) => {
            vec![ThreadUpdate::ToolCall(tool_call_update(
                tool_call,
                cwd,
                collapse_running_commands,
            ))]
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

fn tool_call_update(
    tool_call: &ToolCall,
    cwd: Option<&Path>,
    collapse_running_commands: bool,
) -> acp_v2::ToolCallUpdate {
    let detail = tool_call.detail();
    let diff = edit_diff(&detail, cwd);
    let (title, kind, body) = match (&tool_call.agent_message, &detail) {
        (Some(message), _) => {
            let agent = message
                .sender
                .title
                .as_deref()
                .unwrap_or(&message.sender.id);
            let title = match message.event {
                AgentMessageEvent::Message => format!("Message from agent “{agent}”"),
                AgentMessageEvent::Finished => format!("Agent “{agent}” finished"),
                AgentMessageEvent::Errored => format!("Agent “{agent}” failed"),
                AgentMessageEvent::PermissionRequired => {
                    format!("Agent “{agent}” needs a permission")
                }
                AgentMessageEvent::Closed => format!("Agent “{agent}” closed"),
                AgentMessageEvent::Unknown => format!("Agent “{agent}”"),
            };
            (
                title,
                "other",
                Some(message.text.clone()).filter(|text| !text.is_empty()),
            )
        }
        (None, detail) => match detail {
            ToolCallDetail::Shell {
                command,
                output,
                exit_code,
                ..
            } => shell_tool_call(
                command,
                output.as_deref(),
                *exit_code,
                matches!(tool_call.status, ToolCallStatus::Running) && !collapse_running_commands,
            ),
            ToolCallDetail::Read { file_path, .. } => (format!("Read {file_path}"), "read", None),
            ToolCallDetail::Edit {
                file_path,
                unified_diff,
                ..
            } => (
                format!("Edit {file_path}"),
                "edit",
                // Without a resolvable path the diff can only be shown as text.
                unified_diff
                    .as_deref()
                    .filter(|_| diff.is_none())
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
        },
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
    if let ToolCallDetail::Shell { command, .. } = &detail
        && kind != "execute"
    {
        update["rawInput"] = shell_raw_input(command);
        // Zed's terminal icon for the row (see `render_tool_call_label` in agent_ui).
        update["_meta"] = json!({"pazedo_icon": "terminal"});
    }
    let mut content = Vec::new();
    if let Some(diff) = diff {
        content.push(diff);
    }
    if let Some(body) = body {
        content.push(json!({"type": "content", "content": text_block(&body)}));
    }
    if !content.is_empty() {
        update["content"] = Value::Array(content);
    }
    wire(update)
}

/// The hunks of a one-file unified diff without its file headers. Providers write git headers
/// with paths relative to the repository (`a/src/main.rs`), which Zed compares literally with
/// the absolute path reported for the change; a header-less patch for a single file change is
/// accepted as is. Multi-file diffs are left to the text fallback.
fn single_file_hunks(diff: &str) -> Option<String> {
    let files = diff
        .lines()
        .filter(|line| line.starts_with("+++ ") || line.starts_with("diff --git "))
        .count();
    if files > 2 {
        return None;
    }
    let start = diff.lines().position(|line| line.starts_with("@@"))?;
    Some(diff.lines().skip(start).collect::<Vec<_>>().join("\n"))
}

/// Whether every hunk has as many old and new lines as its header claims.
fn hunks_are_consistent(hunks: &str) -> bool {
    fn count(range: &str) -> Option<u32> {
        match range.split_once(',') {
            Some((_, count)) => count.parse().ok(),
            None => range.parse::<u32>().ok().map(|_| 1),
        }
    }
    let mut expected: Option<(u32, u32)> = None;
    let (mut old, mut new) = (0, 0);
    let mut hunk_count = 0;
    let matches = |expected: Option<(u32, u32)>, old: u32, new: u32| {
        expected
            .is_none_or(|(expected_old, expected_new)| expected_old == old && expected_new == new)
    };
    for line in hunks.lines() {
        if let Some(header) = line.strip_prefix("@@ ") {
            if !matches(expected, old, new) {
                return false;
            }
            let mut ranges = header.split_whitespace();
            let (Some(old_range), Some(new_range)) = (ranges.next(), ranges.next()) else {
                return false;
            };
            let (Some(old_count), Some(new_count)) = (
                old_range.strip_prefix('-').and_then(count),
                new_range.strip_prefix('+').and_then(count),
            ) else {
                return false;
            };
            expected = Some((old_count, new_count));
            (old, new) = (0, 0);
            hunk_count += 1;
        } else if line.starts_with('-') {
            old += 1;
        } else if line.starts_with('+') {
            new += 1;
        } else if line.starts_with(' ') {
            old += 1;
            new += 1;
        } else if !line.starts_with('\\') {
            return false;
        }
    }
    hunk_count > 0 && matches(expected, old, new)
}

/// An edit or write as ACP diff content, which Zed renders as a diff with syntax highlighting.
/// Paseo reports a unified diff, or the replaced and new text; ACP needs an absolute path and a
/// git patch (hunks without file headers are accepted for a single file).
fn edit_diff(detail: &ToolCallDetail, cwd: Option<&Path>) -> Option<Value> {
    let (file_path, operation, patch) = match detail {
        ToolCallDetail::Edit {
            file_path,
            old_string,
            new_string,
            unified_diff,
        } => {
            // A provider's own diff carries real line numbers, but Zed rejects a malformed one
            // outright, so it is only used when its hunks add up.
            let provided = unified_diff
                .as_deref()
                .and_then(single_file_hunks)
                .filter(|hunks| hunks_are_consistent(hunks));
            let patch = match (provided, old_string, new_string) {
                (Some(hunks), _, _) => hunks,
                (None, Some(old), Some(new)) => language::unified_diff(old, new),
                _ => return None,
            };
            (file_path, "modify", patch)
        }
        ToolCallDetail::Write {
            file_path,
            content: Some(content),
        } => (file_path, "add", language::unified_diff("", content)),
        _ => return None,
    };
    if patch.trim().is_empty() {
        return None;
    }
    let path = Path::new(file_path);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd?.join(path)
    };
    Some(json!({
        "type": "diff",
        "changes": [{"operation": operation, "path": path}],
        "patch": {"format": "git_patch", "text": patch},
    }))
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

/// A shell command's title, kind and body. While it runs it is Zed's command card (the
/// `execute` kind, titled with the command). Once it has finished it is one line with the command
/// that expands to its output, as finished thinking collapses: Zed's command card cannot be
/// collapsed, and a thread of commands is long. The whole command is its raw input then (see
/// `shell_raw_input`).
fn shell_tool_call(
    command: &str,
    output: Option<&str>,
    exit_code: Option<i64>,
    running: bool,
) -> (String, &'static str, Option<String>) {
    let output = output.filter(|output| !output.trim().is_empty());
    let exit_line = exit_code
        .filter(|code| *code != 0)
        .map(|code| format!("Exit code {code}"));
    if running {
        let body = output.map(|output| {
            let mut body = format!("```\n{}\n```", truncate(output));
            if let Some(exit_line) = &exit_line {
                body.push('\n');
                body.push_str(exit_line);
            }
            body
        });
        return (command.to_string(), "execute", body);
    }
    const TITLE_LIMIT: usize = 120;
    let first_line = command.lines().next().unwrap_or_default();
    let shortened = command.lines().nth(1).is_some() || first_line.chars().count() > TITLE_LIMIT;
    let summary: String = first_line
        .chars()
        .take(TITLE_LIMIT)
        .collect::<String>()
        .replace('`', "'");
    let title = if shortened {
        format!("`{summary}` …")
    } else {
        format!("`{summary}`")
    };
    let mut parts = Vec::new();
    if let Some(output) = output {
        parts.push(format!("```\n{}\n```", truncate(output)));
    }
    parts.extend(exit_line);
    let body = (!parts.is_empty()).then(|| parts.join("\n\n"));
    (title, "other", body)
}

/// A finished command, shown above its output when it is expanded. A string raw input is shown
/// as markdown.
fn shell_raw_input(command: &str) -> Value {
    Value::String(format!("```sh\n{}\n```", truncate(command)))
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
                ThreadUpdate::EndTurn(stop_reason) => format!("end turn {stop_reason}"),
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
                "end turn end_turn",
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
            vec![
                "state running",
                "state requires_action",
                "end turn end_turn"
            ]
        );
    }

    #[test]
    fn questions_become_a_form_whose_answers_are_keyed_by_header() {
        // A Claude AskUserQuestion request as a 0.15.4 daemon sends it.
        let request: PermissionRequest = serde_json::from_value(json!({
            "id": "permission-1", "provider": "claude", "name": "AskUserQuestion",
            "kind": "question", "title": "Which old backups should I delete?",
            "input": {"questions": [
                {"question": "Which old backups should I delete?", "header": "Backups",
                 "multiSelect": false, "allowOther": true, "options": [
                    {"label": "All but yesterday's", "description": "Keep the newest."},
                    {"label": "All six", "description": "Delete everything."},
                 ]},
                {"question": "Which hosts?", "header": "Hosts", "multiSelect": true,
                 "allowOther": true, "options": [{"label": "nas"}, {"label": "pi"}]},
                {"question": "Anything else?", "header": "Notes", "options": [],
                 "allowEmpty": true},
            ]},
        }))
        .unwrap();
        let questions = request.questions().unwrap();
        assert_eq!(questions.len(), 3);

        let form = serde_json::to_value(question_form("agent-1", &questions, None)).unwrap();
        assert_eq!(form["mode"], "form");
        assert_eq!(form["message"], "Backups · Hosts · Notes");
        let properties = &form["requestedSchema"]["properties"];
        assert_eq!(
            properties["question_00"]["title"],
            "Which old backups should I delete?"
        );
        assert_eq!(
            properties["question_00"]["oneOf"][0],
            json!({"const": "All but yesterday's", "title": "All but yesterday's",
                   "description": "Keep the newest."})
        );
        assert_eq!(properties["question_00_text"]["title"], "Other");
        assert_eq!(properties["question_01"]["type"], "array");
        assert_eq!(properties["question_02_text"]["title"], "Anything else?");
        assert!(properties.get("question_02").is_none());
        assert_eq!(form["requestedSchema"]["required"], json!([]));

        let content =
            |value: Value| -> QuestionFormContent { serde_json::from_value(value).unwrap() };
        assert_eq!(
            question_answers(
                &questions,
                &content(json!({
                    "question_00": "All six",
                    "question_01": ["nas", "pi"],
                    "question_01_text": " router ",
                })),
            ),
            Some(serde_json::Map::from_iter([
                ("Backups".to_string(), json!("All six")),
                ("Hosts".to_string(), json!("nas, pi, router")),
                ("Notes".to_string(), json!("")),
            ]))
        );
        let typed_over_choice = question_answers(
            &questions,
            &content(json!({
                "question_00": "All six",
                "question_00_text": "None of them",
                "question_01": ["pi"],
            })),
        )
        .unwrap();
        assert_eq!(typed_over_choice["Backups"], "None of them");

        let unanswered = content(json!({"question_00": "All six", "question_01_text": "  "}));
        assert_eq!(question_answers(&questions, &unanswered), None);
        let again =
            serde_json::to_value(question_form("agent-1", &questions, Some(&unanswered))).unwrap();
        assert_eq!(
            again["requestedSchema"]["properties"]["question_00"]["default"],
            "All six"
        );
        assert_eq!(
            again["requestedSchema"]["description"],
            "Answer every question."
        );
    }

    #[test]
    fn a_question_without_a_question_list_stays_a_permission_prompt() {
        let request: PermissionRequest = serde_json::from_value(json!({
            "id": "p", "provider": "mock", "name": "ask", "kind": "question",
            "input": {"questions": []},
        }))
        .unwrap();
        assert_eq!(request.questions(), None);
    }

    #[test]
    fn commands_keep_their_argument_hint() {
        let command = |value: Value| {
            serde_json::to_value(available_command(
                serde_json::from_value::<AgentSlashCommand>(value).unwrap(),
            ))
            .unwrap()
        };
        assert_eq!(
            command(json!({
                "name": "compact", "description": "Free up context",
                "argumentHint": "<optional custom summarization instructions>", "kind": "command",
            })),
            json!({
                "type": "command", "name": "compact", "description": "Free up context",
                "input": {"type": "text", "hint": "<optional custom summarization instructions>"},
            })
        );
        assert_eq!(
            command(json!({
                "name": "advisor", "description": "Consult a stronger model", "argumentHint": "",
                "kind": "skill",
            })),
            json!({"type": "command", "name": "advisor", "description": "Consult a stronger model"}),
            "a skill is a command, and an empty hint is no input"
        );
    }

    #[test]
    fn the_end_of_a_replaced_turn_keeps_the_next_turn_running() {
        let mut state = StreamState::default();
        let event = |value: Value| AgentStreamEvent::from_value(&value);
        let mut updates = Vec::new();
        for value in [
            json!({"type": "turn_started", "provider": "mock", "turnId": "t1"}),
            // An interrupting message: Paseo cancels the turn and starts the next.
            json!({"type": "turn_canceled", "provider": "mock", "turnId": "t1",
                   "reason": "replaced"}),
            json!({"type": "turn_started", "provider": "mock", "turnId": "t2"}),
            // Claude ending its turn before taking a steered message.
            json!({"type": "turn_completed", "provider": "claude", "turnId": "t2"}),
            json!({"type": "turn_started", "provider": "claude", "turnId": "t3"}),
            json!({"type": "turn_completed", "provider": "claude", "turnId": "t2"}),
        ] {
            updates.extend(state.updates(&event(value)));
        }
        assert_eq!(
            keys(&updates),
            vec![
                "state running",
                "end turn cancelled",
                "state running",
                "end turn end_turn",
                "state running",
            ],
            "the late end of t2 is dropped while t3 runs"
        );
        assert!(state.turn_active);

        let updates = state.updates(&event(
            json!({"type": "turn_completed", "provider": "claude", "turnId": "t3"}),
        ));
        assert_eq!(keys(&updates), vec!["end turn end_turn"]);
        let updates = state.updates(&event(
            json!({"type": "turn_completed", "provider": "claude", "turnId": "t4"}),
        ));
        assert_eq!(
            keys(&updates),
            vec!["end turn end_turn"],
            "with no turn running, an end from an unseen turn still ends it"
        );
    }

    #[test]
    fn messages_between_agents_are_not_shown_as_tool_calls() {
        let agent_message = |event: &str| {
            TimelineItem::from_value(&json!({
                "type": "tool_call", "callId": format!("paseo-agent-message:{event}"),
                "name": "agent_message", "status": "completed", "error": null,
                "agentMessage": {"event": event, "sender": {"id": "a1", "title": "Planner"},
                                 "text": "Fix the tests."},
                "detail": {"type": "plain_text", "text": "Fix the tests.", "icon": "bot"},
            }))
        };

        let prompt = history_updates(&agent_message("message"), "k", None, false);
        let [ThreadUpdate::User(message)] = prompt.as_slice() else {
            panic!("expected a user message, got {:?}", keys(&prompt));
        };
        assert_eq!(message.message_id.0.as_ref(), "paseo-agent-message:message");
        assert_eq!(
            serde_json::to_value(&message.content).unwrap()[0]["text"],
            "From agent “Planner”:\n\nFix the tests."
        );

        let ThreadUpdate::ToolCall(finished) =
            history_updates(&agent_message("finished"), "k", None, false).remove(0)
        else {
            panic!("expected a tool call");
        };
        let finished = serde_json::to_value(finished).unwrap();
        assert_eq!(finished["title"], "Agent “Planner” finished");
        assert_eq!(finished["content"][0]["content"]["text"], "Fix the tests.");
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
                Some(Path::new("/repo")),
                false,
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
        // A finished command is one line that expands to its output.
        assert_eq!(shell["title"], "`cargo test`");
        assert_eq!(shell["kind"], "other");
        assert_eq!(shell["status"], "failed");
        assert_eq!(
            shell["content"][0]["content"]["text"],
            "```\nok\n```\n\nExit code 1\n\n**Error:** tests failed"
        );

        // A running one is Zed's command card.
        let running = tool_call(
            json!({"type": "shell", "command": "cargo test"}),
            "running",
            json!(null),
        );
        assert_eq!(running["title"], "cargo test");
        assert_eq!(running["kind"], "execute");

        // Unless terminal cards are off, then it is a collapsed row from the start.
        let ThreadUpdate::ToolCall(collapsed) = history_updates(
            &TimelineItem::from_value(&json!({
                "type": "tool_call", "callId": "c", "name": "bash", "status": "running",
                "error": null, "detail": {"type": "shell", "command": "cargo test"},
            })),
            "k",
            None,
            true,
        )
        .remove(0) else {
            panic!("expected a tool call");
        };
        let collapsed = serde_json::to_value(collapsed).unwrap();
        assert_eq!(collapsed["kind"], "other");
        assert_eq!(collapsed["title"], "`cargo test`");

        // A long or multi-line command shows its first line and expands to the whole of it.
        let script = tool_call(
            json!({"type": "shell", "command": "cat <<'EOF'\nhello\nEOF", "output": "hello"}),
            "completed",
            json!(null),
        );
        assert_eq!(script["title"], "`cat <<'EOF'` …");
        assert_eq!(script["rawInput"], "```sh\ncat <<'EOF'\nhello\nEOF\n```");
        assert_eq!(script["content"][0]["content"]["text"], "```\nhello\n```");
        assert_eq!(running.get("rawInput"), None);
        assert_eq!(script["_meta"]["pazedo_icon"], "terminal");

        let edit = tool_call(
            json!({"type": "edit", "filePath": "src/main.rs", "unifiedDiff": "@@ -1 +1 @@\n-a\n+b"}),
            "completed",
            Value::Null,
        );
        assert_eq!(edit["title"], "Edit src/main.rs");
        assert_eq!(edit["kind"], "edit");
        assert_eq!(edit["status"], "completed");
        // Relative paths resolve against the agent's directory; the diff replaces the text body.
        assert_eq!(
            edit["content"],
            json!([{
                "type": "diff",
                "changes": [{"operation": "modify", "path": "/repo/src/main.rs"}],
                "patch": {"format": "git_patch", "text": "@@ -1 +1 @@\n-a\n+b"},
            }])
        );

        let git = tool_call(
            json!({"type": "edit", "filePath": "src/main.rs",
                   "unifiedDiff": "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-a\n+b"}),
            "completed",
            Value::Null,
        );
        assert_eq!(
            git["content"][0]["patch"]["text"], "@@ -1 +1 @@\n-a\n+b",
            "git headers are dropped"
        );
        assert!(hunks_are_consistent("@@ -1,2 +1,2 @@\n a\n-b\n+c"));
        // The mock provider's synthetic diff claims more lines than it has; the diff is then
        // generated from the old and new strings.
        assert!(!hunks_are_consistent("@@ -42,7 +42,9 @@\n a\n-b\n+c"));
        let mock = tool_call(
            json!({"type": "edit", "filePath": "src/main.rs", "oldString": "b\n", "newString": "c\n",
                   "unifiedDiff": "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -42,7 +42,9 @@\n a\n-b\n+c"}),
            "completed",
            Value::Null,
        );
        let patch = mock["content"][0]["patch"]["text"].as_str().unwrap();
        assert!(!patch.contains("@@ -42"), "{patch}");
        assert_eq!(
            single_file_hunks("+++ b/a\n@@ -1 +1 @@\n+++ b/b\n@@ -1 +1 @@\n+++ b/c\n@@"),
            None,
            "multi-file diffs fall back to text"
        );

        let replaced = tool_call(
            json!({"type": "edit", "filePath": "/abs/lib.rs", "oldString": "a\n", "newString": "b\n"}),
            "completed",
            Value::Null,
        );
        assert_eq!(replaced["content"][0]["changes"][0]["path"], "/abs/lib.rs");
        let patch = replaced["content"][0]["patch"]["text"].as_str().unwrap();
        assert!(patch.contains("-a") && patch.contains("+b"), "{patch}");

        let written = tool_call(
            json!({"type": "write", "filePath": "notes.md", "content": "hello\n"}),
            "completed",
            Value::Null,
        );
        assert_eq!(written["content"][0]["changes"][0]["operation"], "add");

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
                selected_action_id: Some("implement".into()),
                updated_input: None,
            }
        );
    }

    #[test]
    fn offers_the_providers_models_and_the_agents_modes() {
        let agent: paseo_client::protocol::Agent = serde_json::from_value(json!({
            "id": "a", "provider": "claude", "cwd": "/repo", "model": null,
            "createdAt": "2026-11-14T10:00:00Z", "updatedAt": "2026-11-14T10:00:00Z",
            "lastUserMessageAt": null, "status": "idle", "capabilities": {},
            "currentModeId": "plan", "persistence": null, "title": null,
            "availableModes": [{"id": "plan", "label": "Plan"}, {"id": "auto", "label": "Auto"}],
        }))
        .unwrap();
        let provider: paseo_client::protocol::ProviderEntry = serde_json::from_value(json!({
            "provider": "claude", "status": "ready", "label": "Claude",
            "models": [
                {"provider": "claude", "id": "opus", "label": "Opus", "isDefault": true},
                {"provider": "claude", "id": "fable", "label": "Fable"},
                {"provider": "claude", "id": "legacy", "label": "Legacy", "isSelectable": false},
            ],
        }))
        .unwrap();
        let options: Vec<Value> = config_options(&agent, Some(&provider))
            .iter()
            .map(|option| serde_json::to_value(option).unwrap())
            .collect();
        assert_eq!(options[0]["configId"], "model");
        assert_eq!(
            options[0]["currentValue"], "opus",
            "no model set means the default"
        );
        assert_eq!(
            options[0]["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|option| option["value"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["opus", "fable"]
        );
        assert_eq!(options[1]["configId"], "mode");
        assert_eq!(options[1]["currentValue"], "plan");

        // A model with thinking levels adds a thought-level selector; its value comes from the
        // agent, else the model's default.
        let provider: paseo_client::protocol::ProviderEntry = serde_json::from_value(json!({
            "provider": "claude", "status": "ready",
            "models": [{"provider": "claude", "id": "opus", "label": "Opus", "isDefault": true,
                        "thinkingOptions": [{"id": "low", "label": "Low"}, {"id": "high", "label": "High"}],
                        "defaultThinkingOptionId": "low"}],
        }))
        .unwrap();
        let thinking = |agent: &paseo_client::protocol::Agent| -> Value {
            config_options(agent, Some(&provider))
                .iter()
                .map(|option| serde_json::to_value(option).unwrap())
                .find(|option| option["configId"] == "thinking")
                .unwrap()
        };
        assert_eq!(thinking(&agent)["category"], "thought_level");
        assert_eq!(thinking(&agent)["currentValue"], "low");
        let mut thinking_agent = agent.clone();
        thinking_agent.effective_thinking_option_id = Some("high".into());
        assert_eq!(thinking(&thinking_agent)["currentValue"], "high");

        // Without a provider snapshot the agent's model is still shown.
        let mut agent = agent;
        agent.model = Some("custom".into());
        let options = config_options(&agent, None);
        assert_eq!(
            serde_json::to_value(&options[0]).unwrap()["currentValue"],
            "custom"
        );
    }

    #[test]
    fn images_are_sent_and_kept_in_the_echo() {
        let image = wire::<acp_v2::ContentBlock>(
            json!({"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"}),
        );
        let content = vec![wire(text_block("What is this?")), image.clone()];
        assert_eq!(prompt_text(&content), "What is this?");
        let images = prompt_images(&content);
        assert_eq!(
            images,
            [ImageAttachment {
                data: "iVBORw0KGgo=".into(),
                mime_type: "image/png".into()
            }]
        );
        let request = serde_json::to_value(SendAgentMessage {
            agent_id: "a1".into(),
            text: "What is this?".into(),
            message_id: None,
            active_turn_behavior: None,
            images,
        })
        .unwrap();
        assert_eq!(
            request["images"],
            json!([{"data": "iVBORw0KGgo=", "mimeType": "image/png"}])
        );

        // Paseo's echo has the text only; the thread's message keeps the image.
        let mut updates = history_updates(
            &TimelineItem::UserMessage {
                text: "What is this?".into(),
                message_id: Some("m1".into()),
            },
            "k",
            None,
            false,
        );
        let sent_images = HashMap::from_iter([("m1".to_string(), vec![image])]);
        restore_sent_images(&mut updates, &sent_images);
        let ThreadUpdate::User(message) = &updates[0] else {
            panic!("expected the user message, got {updates:?}");
        };
        let MaybeUndefined::Value(content) = &message.content else {
            panic!("the message has no content");
        };
        assert!(matches!(
            content.as_slice(),
            [_, acp_v2::ContentBlock::Image(_)]
        ));
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
