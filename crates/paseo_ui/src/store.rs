use anyhow::Result;
use collections::{BTreeMap, HashMap};
use futures::{FutureExt as _, StreamExt as _};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task};
use paseo_client::{
    Client, ConnectOptions, Endpoint, Event, Sleep,
    protocol::{
        Agent, AgentScope, AgentStatus, AgentStream, AgentTimelineReplacement, AgentUpdate,
        FetchAgents, FetchWorkspaces, ListProjects, Page, ProjectSummary, ProjectUpdate,
        ServerInfo, ServerMessage, SetSessionEventSubscription, Subscribe, Workspace,
        WorkspaceUpdate,
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

/// For open agent sessions, which follow their own timeline subscriptions.
#[derive(Clone, Debug)]
pub enum PaseoStoreEvent {
    Connected,
    Disconnected,
    AgentStream(AgentStream),
    AgentTimelineReplacement(AgentTimelineReplacement),
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
    subscriptions: DirectorySubscriptions,
    /// Pushes that arrive while a snapshot is being fetched. Their order relative to the
    /// snapshot response is not preserved between the two foreground tasks, so they are
    /// applied after the snapshot.
    queued_messages: Option<Vec<ServerMessage>>,
    sync_task: Option<Task<()>>,
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
            subscriptions: DirectorySubscriptions::default(),
            queued_messages: None,
            sync_task: None,
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
                cx.emit(PaseoStoreEvent::Connected);
            }
            Event::Disconnected { reason } => {
                // Keep the last known directory on screen; the next sync replaces it.
                self.status = ConnectionStatus::Disconnected(reason.into());
                self.subscriptions = DirectorySubscriptions::default();
                self.queued_messages = None;
                self.sync_task = None;
                cx.emit(PaseoStoreEvent::Disconnected);
            }
            Event::Message(ServerMessage::AgentStream(stream)) => {
                cx.emit(PaseoStoreEvent::AgentStream(stream));
            }
            Event::Message(ServerMessage::AgentTimelineReplacement(replacement)) => {
                cx.emit(PaseoStoreEvent::AgentTimelineReplacement(replacement));
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
                cx.notify();
            })
            .ok();
        }));
    }

    pub(crate) fn begin_sync(&mut self) {
        self.subscriptions = DirectorySubscriptions::default();
        self.queued_messages = Some(Vec::new());
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
