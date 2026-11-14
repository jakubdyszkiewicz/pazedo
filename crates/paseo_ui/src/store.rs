use anyhow::{Context as _, Result};
use collections::{BTreeMap, HashMap};
use futures::{FutureExt as _, StreamExt as _};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task};
use paseo_client::{
    Client, ConnectOptions, Endpoint, Event, Sleep,
    protocol::{
        Agent, AgentScope, AgentStatus, AgentStream, AgentTimelineReplacement, AgentUpdate,
        CreateTerminal, FetchAgents, FetchWorkspaces, GetProvidersSnapshot, ListProjects, Page,
        ProjectSummary, ProjectUpdate, ProviderEntry, ServerInfo, ServerMessage,
        SetSessionEventSubscription, Subscribe, SubscribeTerminals, TerminalInfo,
        TerminalStreamExit, TerminalsChanged, Workspace, WorkspaceUpdate,
    },
};
use std::{cmp::Reverse, sync::Arc};

/// The daemon to connect to, as `host:port`, `ws://...` or a socket path. Until pazedo has
/// settings of its own, nothing connects unless this is set, so a development build never
/// talks to the user's real daemon by accident.
pub const ENDPOINT_ENV_VAR: &str = "PAZEDO_PASEO_ENDPOINT";

const CLIENT_ID: &str = "pazedo";
const PAGE_SIZE: u32 = 200;
const SESSION_EVENTS: &[&str] = &[
    "project.update",
    "agent_attention_required",
    "agent_permission_request",
    "agent_permission_resolved",
];

#[derive(Clone, Debug)]
pub enum ConnectionStatus {
    NotConfigured,
    InvalidEndpoint(SharedString),
    Connecting(Endpoint),
    Connected(Arc<ServerInfo>),
    Disconnected(SharedString),
}

/// For open agent sessions and terminals, which follow their own subscriptions.
#[derive(Clone, Debug)]
pub enum PaseoStoreEvent {
    Connected,
    Disconnected,
    AgentStream(AgentStream),
    AgentTimelineReplacement(AgentTimelineReplacement),
    TerminalStreamExit(TerminalStreamExit),
}

struct GlobalPaseoStore(Entity<PaseoStore>);

impl Global for GlobalPaseoStore {}

/// The live directory of one Paseo daemon: projects, workspaces and agents, kept current through
/// subscriptions that are re-established after every reconnect.
pub struct PaseoStore {
    client: Option<Client>,
    status: ConnectionStatus,
    sync_error: Option<SharedString>,
    projects: BTreeMap<String, ProjectSummary>,
    workspaces: HashMap<String, Workspace>,
    agents: HashMap<String, Agent>,
    providers: Vec<ProviderEntry>,
    providers_task: Option<Task<()>>,
    subscriptions: DirectorySubscriptions,
    /// Pushes that arrive while a snapshot is being fetched. Their order relative to the
    /// snapshot response is not preserved between the two foreground tasks, so they are
    /// applied after the snapshot.
    queued_messages: Option<Vec<ServerMessage>>,
    sync_task: Option<Task<()>>,
    /// Terminal lists are subscribed per workspace, only for workspaces the user opened, by
    /// workspace id. The value is the current subscription id once the daemon answered.
    terminal_subscriptions: HashMap<String, Option<String>>,
    terminals: HashMap<String, Vec<TerminalInfo>>,
    terminal_tasks: HashMap<String, Task<()>>,
    _tasks: Vec<Task<()>>,
}

#[derive(Default)]
pub(crate) struct DirectorySubscriptions {
    pub(crate) workspaces: Option<String>,
    pub(crate) agents: Option<String>,
}

impl DirectorySubscriptions {
    /// Drops pushes from subscriptions of an earlier connection or sync.
    fn accepts(current: &Option<String>, pushed: &Option<String>) -> bool {
        match (current, pushed) {
            (Some(current), Some(pushed)) => current == pushed,
            _ => true,
        }
    }
}

pub(crate) struct DirectorySnapshot {
    pub(crate) projects: Vec<ProjectSummary>,
    pub(crate) workspaces: Vec<Workspace>,
    pub(crate) agents: Vec<Agent>,
    pub(crate) subscriptions: DirectorySubscriptions,
}

pub struct ProjectGroup {
    pub project_id: String,
    pub name: SharedString,
    pub root_path: SharedString,
    pub workspaces: Vec<WorkspaceEntry>,
}

pub struct WorkspaceEntry {
    pub workspace: Workspace,
    /// Newest first.
    pub agents: Vec<Agent>,
    /// Empty until the workspace's terminals are watched.
    pub terminals: Vec<TerminalInfo>,
}

impl WorkspaceEntry {
    pub fn running_agent_count(&self) -> usize {
        self.agents
            .iter()
            .filter(|agent| {
                matches!(
                    agent.status,
                    AgentStatus::Running | AgentStatus::Initializing
                )
            })
            .count()
    }

    pub fn pending_permission_count(&self) -> usize {
        self.agents
            .iter()
            .map(|agent| agent.pending_permissions.len())
            .sum()
    }
}

impl PaseoStore {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(store) = cx.try_global::<GlobalPaseoStore>() {
            return store.0.clone();
        }
        let endpoint = std::env::var(ENDPOINT_ENV_VAR).ok();
        let store = cx.new(|cx| Self::new(endpoint, cx));
        cx.set_global(GlobalPaseoStore(store.clone()));
        store
    }

    pub fn new(endpoint: Option<String>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            client: None,
            status: ConnectionStatus::NotConfigured,
            sync_error: None,
            projects: BTreeMap::default(),
            workspaces: HashMap::default(),
            agents: HashMap::default(),
            providers: Vec::new(),
            providers_task: None,
            subscriptions: DirectorySubscriptions::default(),
            queued_messages: None,
            sync_task: None,
            terminal_subscriptions: HashMap::default(),
            terminals: HashMap::default(),
            terminal_tasks: HashMap::default(),
            _tasks: Vec::new(),
        };
        let Some(endpoint) = endpoint.filter(|endpoint| !endpoint.trim().is_empty()) else {
            return this;
        };
        let endpoint = match Endpoint::parse(&endpoint) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                this.status = ConnectionStatus::InvalidEndpoint(format!("{error:#}").into());
                return this;
            }
        };

        let executor = cx.background_executor().clone();
        let sleep: Sleep = Arc::new(move |duration| executor.timer(duration).boxed());
        let mut options = ConnectOptions::new(endpoint.clone(), CLIENT_ID, sleep);
        options.app_version = Some(format!("pazedo {}", env!("CARGO_PKG_VERSION")));
        let (client, mut events, driver) = Client::new(options);
        this._tasks.push(cx.background_spawn(driver));
        this._tasks.push(cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                if this
                    .update(cx, |this, cx| this.handle_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
        this.client = Some(client);
        this.status = ConnectionStatus::Connecting(endpoint);
        this
    }

    /// The client, once the store has been configured with an endpoint. Requests fail while
    /// disconnected.
    pub fn client(&self) -> Option<Client> {
        self.client.clone()
    }

    pub fn agent(&self, id: &str) -> Option<&Agent> {
        self.agents.get(id)
    }

    /// Providers that can start agents, as last reported by the daemon.
    pub fn providers(&self) -> &[ProviderEntry] {
        &self.providers
    }

    pub fn provider(&self, provider: &str) -> Option<&ProviderEntry> {
        self.providers
            .iter()
            .find(|entry| entry.provider == provider)
    }

    /// Fetches the provider snapshot again. Providers report `loading` until they have probed
    /// their models, so callers refresh before showing choices.
    pub fn refresh_providers(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone().filter(Client::is_connected) else {
            return;
        };
        self.providers_task = Some(cx.spawn(async move |this, cx| {
            match client.request(&GetProvidersSnapshot::default()).await {
                Ok(snapshot) => {
                    this.update(cx, |this, cx| {
                        this.providers = snapshot.entries;
                        cx.notify();
                    })
                    .ok();
                }
                Err(error) => log::warn!("failed to load Paseo providers: {error:#}"),
            }
        }));
    }

    /// Several Paseo workspaces can share a directory; new agents go to the newest one.
    pub fn newest_workspace_for_directory(
        &self,
        directory: &std::path::Path,
    ) -> Option<&Workspace> {
        self.workspaces
            .values()
            .filter(|workspace| std::path::Path::new(workspace.directory()) == directory)
            .max_by(|a, b| a.created_at.cmp(&b.created_at))
    }

    pub fn status(&self) -> &ConnectionStatus {
        &self.status
    }

    pub fn sync_error(&self) -> Option<&SharedString> {
        self.sync_error.as_ref()
    }

    pub fn workspace(&self, id: &str) -> Option<&Workspace> {
        self.workspaces.get(id)
    }

    pub fn is_watching_terminals(&self, workspace_id: &str) -> bool {
        self.terminal_subscriptions.contains_key(workspace_id)
    }

    /// Keeps the workspace's terminal list current from now on, across reconnects.
    pub fn watch_terminals(&mut self, workspace_id: &str, cx: &mut Context<Self>) {
        if self.is_watching_terminals(workspace_id) {
            return;
        }
        self.terminal_subscriptions
            .insert(workspace_id.to_string(), None);
        self.subscribe_terminals(workspace_id.to_string(), cx);
    }

    fn subscribe_terminals(&mut self, workspace_id: String, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone().filter(Client::is_connected) else {
            return;
        };
        let Some(workspace) = self.workspaces.get(&workspace_id) else {
            return;
        };
        let request = SubscribeTerminals {
            cwd: workspace.directory().to_string(),
            workspace_id: Some(workspace_id.clone()),
        };
        let task = cx.spawn({
            let workspace_id = workspace_id.clone();
            async move |this, cx| match client.request(&request).await {
                Ok(changed) => {
                    this.update(cx, |this, cx| {
                        if let Some(subscription) =
                            this.terminal_subscriptions.get_mut(&workspace_id)
                        {
                            *subscription = changed.subscription_id.clone();
                        }
                        this.terminals.insert(workspace_id, changed.terminals);
                        cx.notify();
                    })
                    .ok();
                }
                Err(error) => {
                    log::warn!("failed to load the terminals of {workspace_id}: {error:#}")
                }
            }
        });
        self.terminal_tasks.insert(workspace_id, task);
    }

    /// Starts a shell in the workspace's directory.
    pub fn create_terminal(
        &mut self,
        workspace_id: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<TerminalInfo>> {
        let Some(client) = self.client.clone() else {
            return Task::ready(Err(anyhow::anyhow!("not connected to a Paseo daemon")));
        };
        let Some(workspace) = self.workspaces.get(workspace_id) else {
            return Task::ready(Err(anyhow::anyhow!("unknown workspace {workspace_id}")));
        };
        let request = CreateTerminal {
            cwd: workspace.directory().to_string(),
            workspace_id: Some(workspace_id.to_string()),
            ..Default::default()
        };
        let workspace_id = workspace_id.to_string();
        cx.spawn(async move |this, cx| {
            let response = client.request(&request).await?;
            let terminal = response.terminal.with_context(|| {
                format!(
                    "the daemon did not create a terminal: {}",
                    response.error.as_deref().unwrap_or("no reason given")
                )
            })?;
            this.update(cx, |this, cx| {
                let terminals = this.terminals.entry(workspace_id).or_default();
                if !terminals.iter().any(|known| known.id == terminal.id) {
                    terminals.push(terminal.clone());
                    cx.notify();
                }
            })?;
            Ok(terminal)
        })
    }

    pub fn terminal(&self, terminal_id: &str) -> Option<&TerminalInfo> {
        self.terminals
            .values()
            .flatten()
            .find(|terminal| terminal.id == terminal_id)
    }

    /// Projects sorted by name; each with its workspaces, pinned first, then newest first.
    pub fn project_groups(&self) -> Vec<ProjectGroup> {
        let mut agents_by_workspace: HashMap<&str, Vec<&Agent>> = HashMap::default();
        for agent in self.agents.values() {
            if let Some(workspace_id) = agent.workspace_id.as_deref() {
                agents_by_workspace
                    .entry(workspace_id)
                    .or_default()
                    .push(agent);
            }
        }

        let mut groups: BTreeMap<&str, ProjectGroup> = self
            .projects
            .values()
            .map(|project| {
                (
                    project.project_id.as_str(),
                    ProjectGroup {
                        project_id: project.project_id.clone(),
                        name: project.project_display_name.clone().into(),
                        root_path: project.project_root_path.clone().into(),
                        workspaces: Vec::new(),
                    },
                )
            })
            .collect();

        for workspace in self.workspaces.values() {
            let mut agents: Vec<Agent> = agents_by_workspace
                .get(workspace.id.as_str())
                .map(|agents| agents.iter().map(|agent| (*agent).clone()).collect())
                .unwrap_or_default();
            agents.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
            let entry = WorkspaceEntry {
                workspace: workspace.clone(),
                agents,
                terminals: self
                    .terminals
                    .get(&workspace.id)
                    .cloned()
                    .unwrap_or_default(),
            };
            // A workspace can arrive before the `project.update` that announces its project.
            groups
                .entry(workspace.project_id.as_str())
                .or_insert_with(|| ProjectGroup {
                    project_id: workspace.project_id.clone(),
                    name: workspace.project_display_name.clone().into(),
                    root_path: workspace.project_root_path.clone().into(),
                    workspaces: Vec::new(),
                })
                .workspaces
                .push(entry);
        }

        let mut groups: Vec<ProjectGroup> = groups.into_values().collect();
        for group in &mut groups {
            group.workspaces.sort_by(|a, b| {
                let a = &a.workspace;
                let b = &b.workspace;
                Reverse(&a.pinned_at)
                    .cmp(&Reverse(&b.pinned_at))
                    .then_with(|| Reverse(&a.created_at).cmp(&Reverse(&b.created_at)))
                    .then_with(|| a.id.cmp(&b.id))
            });
        }
        groups.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.project_id.cmp(&b.project_id))
        });
        groups
    }

    pub(crate) fn handle_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Connected(server_info) => {
                self.status = ConnectionStatus::Connected(server_info);
                self.sync(cx);
                self.refresh_providers(cx);
                cx.emit(PaseoStoreEvent::Connected);
            }
            Event::Disconnected { reason } => {
                // Keep the last known directory on screen; the next sync replaces it.
                self.status = ConnectionStatus::Disconnected(reason.into());
                self.subscriptions = DirectorySubscriptions::default();
                self.queued_messages = None;
                self.sync_task = None;
                self.terminal_tasks.clear();
                for subscription in self.terminal_subscriptions.values_mut() {
                    *subscription = None;
                }
                cx.emit(PaseoStoreEvent::Disconnected);
            }
            Event::Message(ServerMessage::AgentStream(stream)) => {
                cx.emit(PaseoStoreEvent::AgentStream(stream));
            }
            Event::Message(ServerMessage::AgentTimelineReplacement(replacement)) => {
                cx.emit(PaseoStoreEvent::AgentTimelineReplacement(replacement));
            }
            Event::Message(ServerMessage::TerminalsChanged(changed)) => {
                self.apply_terminals_changed(changed);
            }
            Event::Message(ServerMessage::TerminalStreamExit(exit)) => {
                cx.emit(PaseoStoreEvent::TerminalStreamExit(exit));
            }
            Event::Message(message) => match &mut self.queued_messages {
                Some(queued) => queued.push(message),
                None => self.apply_message(message),
            },
            Event::Terminal(_) => {}
        }
        cx.notify();
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.begin_sync();
        self.sync_task = Some(cx.spawn(async move |this, cx| {
            let result = fetch_directory(&client).await;
            this.update(cx, |this, cx| {
                this.finish_sync(result);
                this.resubscribe_terminals(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    pub(crate) fn begin_sync(&mut self) {
        self.subscriptions = DirectorySubscriptions::default();
        self.queued_messages = Some(Vec::new());
    }

    /// Watched workspaces lose their terminal subscriptions with the connection.
    fn resubscribe_terminals(&mut self, cx: &mut Context<Self>) {
        let workspace_ids: Vec<String> = self
            .terminal_subscriptions
            .iter()
            .filter(|(workspace_id, subscription)| {
                subscription.is_none() && !self.terminal_tasks.contains_key(*workspace_id)
            })
            .map(|(workspace_id, _)| workspace_id.clone())
            .collect();
        for workspace_id in workspace_ids {
            self.subscribe_terminals(workspace_id, cx);
        }
    }

    fn apply_terminals_changed(&mut self, changed: TerminalsChanged) {
        let Some(workspace_id) = changed.workspace_id else {
            return;
        };
        let Some(current) = self.terminal_subscriptions.get(&workspace_id) else {
            return;
        };
        if DirectorySubscriptions::accepts(current, &changed.subscription_id) {
            self.terminals.insert(workspace_id, changed.terminals);
        }
    }

    pub(crate) fn finish_sync(&mut self, result: Result<DirectorySnapshot>) {
        let queued = self.queued_messages.take().unwrap_or_default();
        match result {
            Ok(snapshot) => {
                log::info!(
                    "loaded the Paseo directory: {} projects, {} workspaces, {} agents",
                    snapshot.projects.len(),
                    snapshot.workspaces.len(),
                    snapshot.agents.len()
                );
                self.sync_error = None;
                self.apply_snapshot(snapshot);
                for message in queued {
                    self.apply_message(message);
                }
            }
            Err(error) => {
                log::error!("failed to load the Paseo directory: {error:#}");
                self.sync_error = Some(format!("{error:#}").into());
            }
        }
    }

    fn apply_snapshot(&mut self, snapshot: DirectorySnapshot) {
        self.projects = snapshot
            .projects
            .into_iter()
            .map(|project| (project.project_id.clone(), project))
            .collect();
        self.workspaces = snapshot
            .workspaces
            .into_iter()
            .map(|workspace| (workspace.id.clone(), workspace))
            .collect();
        self.agents = snapshot
            .agents
            .into_iter()
            .map(|agent| (agent.id.clone(), agent))
            .collect();
        self.subscriptions = snapshot.subscriptions;
    }

    fn apply_message(&mut self, message: ServerMessage) {
        match message {
            ServerMessage::WorkspaceUpdate(WorkspaceUpdate::Upsert {
                subscription_id,
                workspace,
            }) => {
                if DirectorySubscriptions::accepts(&self.subscriptions.workspaces, &subscription_id)
                {
                    self.workspaces.insert(workspace.id.clone(), workspace);
                }
            }
            ServerMessage::WorkspaceUpdate(WorkspaceUpdate::Remove {
                subscription_id,
                id,
                empty_project,
                removed_project_id,
            }) => {
                if DirectorySubscriptions::accepts(&self.subscriptions.workspaces, &subscription_id)
                {
                    self.workspaces.remove(&id);
                    if let Some(project) = empty_project {
                        self.projects.insert(project.project_id.clone(), project);
                    }
                    if let Some(project_id) = removed_project_id {
                        self.projects.remove(&project_id);
                    }
                }
            }
            ServerMessage::AgentUpdate(AgentUpdate::Upsert {
                subscription_id,
                agent,
                ..
            }) => {
                if DirectorySubscriptions::accepts(&self.subscriptions.agents, &subscription_id) {
                    self.agents.insert(agent.id.clone(), agent);
                }
            }
            ServerMessage::AgentUpdate(AgentUpdate::Remove {
                subscription_id,
                agent_id,
            }) => {
                if DirectorySubscriptions::accepts(&self.subscriptions.agents, &subscription_id) {
                    self.agents.remove(&agent_id);
                }
            }
            ServerMessage::ProjectUpdate(ProjectUpdate::Upsert { project, .. }) => {
                self.projects.insert(project.project_id.clone(), project);
            }
            ServerMessage::ProjectUpdate(ProjectUpdate::Remove { project_id, .. }) => {
                self.projects.remove(&project_id);
            }
            // Attention and permission changes also arrive as `agent_update` upserts, which
            // carry the state the sidebar renders.
            _ => {}
        }
    }
}

/// Lets tests and the screenshot runner show a scripted directory without a daemon.
#[cfg(any(test, feature = "test-support"))]
impl PaseoStore {
    pub fn set_global(store: Entity<Self>, cx: &mut App) {
        cx.set_global(GlobalPaseoStore(store));
    }

    pub fn load_fixture(
        &mut self,
        status: ConnectionStatus,
        projects: Vec<ProjectSummary>,
        workspaces: Vec<Workspace>,
        agents: Vec<Agent>,
        cx: &mut Context<Self>,
    ) {
        self.status = status;
        self.begin_sync();
        self.finish_sync(Ok(DirectorySnapshot {
            projects,
            workspaces,
            agents,
            subscriptions: DirectorySubscriptions::default(),
        }));
        cx.notify();
    }

    pub fn set_providers(&mut self, providers: Vec<ProviderEntry>, cx: &mut Context<Self>) {
        self.providers = providers;
        cx.notify();
    }

    pub fn set_terminals(
        &mut self,
        workspace_id: &str,
        terminals: Vec<TerminalInfo>,
        cx: &mut Context<Self>,
    ) {
        self.terminal_subscriptions
            .insert(workspace_id.to_string(), None);
        self.terminals.insert(workspace_id.to_string(), terminals);
        cx.notify();
    }

    pub fn set_sync_error(&mut self, error: Option<SharedString>, cx: &mut Context<Self>) {
        self.sync_error = error;
        cx.notify();
    }
}

impl EventEmitter<PaseoStoreEvent> for PaseoStore {}

async fn fetch_directory(client: &Client) -> Result<DirectorySnapshot> {
    // Subscribe to project events first so a project created during the fetch is not missed.
    client
        .request(&SetSessionEventSubscription {
            events: SESSION_EVENTS
                .iter()
                .map(|event| event.to_string())
                .collect(),
            notifications: None,
        })
        .await?;
    let projects = client.request(&ListProjects {}).await?.projects;

    let first_page = client
        .request(&FetchWorkspaces {
            include_background: Some(true),
            page: Some(Page {
                limit: PAGE_SIZE,
                cursor: None,
            }),
            subscribe: Some(Subscribe {}),
        })
        .await?;
    let workspaces_subscription = first_page.subscription_id;
    let mut workspaces = first_page.entries;
    let mut projects_without_workspaces = first_page.empty_projects;
    let mut next_cursor = first_page
        .page_info
        .has_more
        .then_some(first_page.page_info.next_cursor)
        .flatten();
    while let Some(cursor) = next_cursor.take() {
        let page = client
            .request(&FetchWorkspaces {
                include_background: Some(true),
                page: Some(Page {
                    limit: PAGE_SIZE,
                    cursor: Some(cursor),
                }),
                subscribe: None,
            })
            .await?;
        workspaces.extend(page.entries);
        projects_without_workspaces.extend(page.empty_projects);
        next_cursor = page
            .page_info
            .has_more
            .then_some(page.page_info.next_cursor)
            .flatten();
    }

    let first_page = client
        .request(&FetchAgents {
            scope: Some(AgentScope::Active),
            include_background: Some(true),
            page: Some(Page {
                limit: PAGE_SIZE,
                cursor: None,
            }),
            subscribe: Some(Subscribe {}),
            ..Default::default()
        })
        .await?;
    let agents_subscription = first_page.subscription_id;
    let mut agents: Vec<Agent> = first_page
        .entries
        .into_iter()
        .map(|entry| entry.agent)
        .collect();
    let mut next_cursor = first_page
        .page_info
        .has_more
        .then_some(first_page.page_info.next_cursor)
        .flatten();
    while let Some(cursor) = next_cursor.take() {
        let page = client
            .request(&FetchAgents {
                scope: Some(AgentScope::Active),
                include_background: Some(true),
                page: Some(Page {
                    limit: PAGE_SIZE,
                    cursor: Some(cursor),
                }),
                ..Default::default()
            })
            .await?;
        agents.extend(page.entries.into_iter().map(|entry| entry.agent));
        next_cursor = page
            .page_info
            .has_more
            .then_some(page.page_info.next_cursor)
            .flatten();
    }

    let mut all_projects = projects;
    for project in projects_without_workspaces {
        if !all_projects
            .iter()
            .any(|known| known.project_id == project.project_id)
        {
            all_projects.push(project);
        }
    }

    Ok(DirectorySnapshot {
        projects: all_projects,
        workspaces,
        agents,
        subscriptions: DirectorySubscriptions {
            workspaces: workspaces_subscription,
            agents: agents_subscription,
        },
    })
}
