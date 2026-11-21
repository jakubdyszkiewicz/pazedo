use anyhow::{Context as _, Result, anyhow, bail};
use collections::HashSet;
use collections::{BTreeMap, HashMap};
use futures::{FutureExt as _, StreamExt as _};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Subscription, Task,
};
use paseo_client::{
    Client, ConnectOptions, Endpoint, Event, Sleep,
    protocol::{
        AddProject, Agent, AgentScope, AgentStatus, AgentStream, AgentTimelineReplacement,
        AgentUpdate, ArchiveWorkspace, AttentionReason, BackgroundWorkItem, BranchSuggestions,
        ClearAgentAttention, CloneGithubProject, CloneProtocol, CloseItems, CreateTerminal,
        CreateWorkspace, DirectorySuggestions, FetchAgents, FetchWorkspaces, GetProvidersSnapshot,
        GithubRepository, HelloAuth, ListBackgroundWork, ListProjects, ListUsageReports, Page,
        ProjectSummary, ProjectUpdate, ProviderEntry, RefreshAgent, RemoveProject, RenameAgent,
        RenameTerminal, SearchGithubRepositories, ServerInfo, ServerMessage,
        SetSessionEventSubscription, SetWorkspaceTitle, StopBackgroundWork, Subscribe,
        SubscribeTerminals, TerminalInfo, TerminalStreamExit, TerminalsChanged, UsageReportEntry,
        Workspace, WorkspaceSource, WorkspaceUpdate,
    },
};
use project::Project;
use recent_projects::RemoteSettings;

use crate::daemon_auth;
use remote::{RemoteConnectionOptions, SshConnectionOptions};
use settings::{RegisterSetting, Settings, SettingsContent, SettingsStore};
use std::{
    cmp::Reverse,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Overrides the daemons in the settings with a single one, e.g. so that
/// `script/pazedo/run-dev` points a development build at the test daemon whatever the shared Zed
/// settings say.
pub const ENDPOINT_ENV_VAR: &str = "PAZEDO_PASEO_ENDPOINT";
/// The `ssh` of the daemon from [`ENDPOINT_ENV_VAR`]; the settings describe other daemons.
pub const SSH_ENV_VAR: &str = "PAZEDO_PASEO_SSH";

/// One configured daemon.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DaemonConfig {
    pub name: Option<String>,
    pub endpoint: Option<String>,
    /// The SSH host the daemon's folders are on, for a daemon on another machine.
    pub ssh: Option<String>,
}

impl DaemonConfig {
    pub fn local(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: Some(endpoint.into()),
            ..Self::default()
        }
    }
}

/// The `paseo` section of the settings: its daemons, the top-level `endpoint` and `ssh` first.
#[derive(Clone, Debug, Default, PartialEq, RegisterSetting)]
pub struct PaseoSettings {
    pub daemons: Vec<DaemonConfig>,
    /// Where the Paseo panel docks; left when unset.
    pub dock: Option<settings::DockPosition>,
}

impl Settings for PaseoSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let non_empty =
            |value: &Option<String>| value.clone().filter(|value| !value.trim().is_empty());
        let Some(paseo) = content.paseo.as_ref() else {
            return Self::default();
        };
        let top_level = DaemonConfig {
            name: non_empty(&paseo.name),
            endpoint: non_empty(&paseo.endpoint),
            ssh: non_empty(&paseo.ssh),
        };
        let listed = paseo.daemons.iter().flatten().map(|daemon| DaemonConfig {
            name: non_empty(&daemon.name),
            endpoint: non_empty(&daemon.endpoint),
            ssh: non_empty(&daemon.ssh),
        });
        Self {
            daemons: std::iter::once(top_level)
                .chain(listed)
                .filter(|daemon| daemon.endpoint.is_some())
                .collect(),
            dock: paseo.dock,
        }
    }
}

fn non_empty_env_var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// The daemons to connect to: the one from the environment variables if set, else the settings'.
fn configured_daemons(cx: &App) -> Vec<DaemonConfig> {
    match non_empty_env_var(ENDPOINT_ENV_VAR) {
        Some(endpoint) => vec![DaemonConfig {
            name: None,
            endpoint: Some(endpoint),
            ssh: non_empty_env_var(SSH_ENV_VAR),
        }],
        None => PaseoSettings::get_global(cx).daemons.clone(),
    }
}

/// Every configured daemon's store, in settings order, kept in step with the settings: a
/// daemon whose endpoint stays keeps its store and connection.
pub struct PaseoDaemons {
    stores: Vec<Entity<PaseoStore>>,
    _settings_subscription: Option<Subscription>,
}

struct GlobalPaseoDaemons(Entity<PaseoDaemons>);

impl Global for GlobalPaseoDaemons {}

impl PaseoDaemons {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(daemons) = cx.try_global::<GlobalPaseoDaemons>() {
            return daemons.0.clone();
        }
        let daemons = cx.new(|cx| {
            let mut this = Self {
                stores: Vec::new(),
                _settings_subscription: None,
            };
            this.reconcile(configured_daemons(cx), cx);
            this._settings_subscription = Some(cx.observe_global::<SettingsStore>(|this, cx| {
                this.reconcile(configured_daemons(cx), cx)
            }));
            this
        });
        cx.set_global(GlobalPaseoDaemons(daemons.clone()));
        daemons
    }

    pub fn stores(&self) -> &[Entity<PaseoStore>] {
        &self.stores
    }

    fn reconcile(&mut self, configs: Vec<DaemonConfig>, cx: &mut Context<Self>) {
        let mut previous = std::mem::take(&mut self.stores);
        for config in configs {
            let existing = previous
                .iter()
                .position(|store| store.read(cx).config.endpoint == config.endpoint);
            match existing {
                Some(index) => {
                    let store = previous.remove(index);
                    store.update(cx, |store, cx| store.set_config(config, cx));
                    self.stores.push(store);
                }
                None => self.stores.push(cx.new(|cx| PaseoStore::new(config, cx))),
            }
        }
        // Open tabs may keep a removed daemon's store alive; it must not stay connected.
        for store in previous {
            store.update(cx, |store, cx| store.connect(None, cx));
        }
        cx.notify();
    }

    /// The daemon and Paseo workspace for a Zed workspace's single folder: among the daemons
    /// whose folders are on the project's machine, the first with a workspace there.
    pub fn workspace_for_folder(
        &self,
        project: &Project,
        directory: &Path,
        cx: &App,
    ) -> Option<(Entity<PaseoStore>, String)> {
        self.stores.iter().find_map(|store| {
            let store_ref = store.read(cx);
            if !store_ref.is_on_host(project, cx) {
                return None;
            }
            let workspace = store_ref.workspace_for_directory(directory)?;
            Some((store.clone(), workspace.id.clone()))
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_global_for_test(stores: Vec<Entity<PaseoStore>>, cx: &mut App) {
        let daemons = cx.new(|_| Self {
            stores,
            _settings_subscription: None,
        });
        cx.set_global(GlobalPaseoDaemons(daemons));
    }
}

const CLIENT_ID: &str = "pazedo";
const USAGE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const PAGE_SIZE: u32 = 200;
const SESSION_EVENTS: &[&str] = &[
    "project.update",
    "agent_attention_required",
    "agent_permission_request",
    "agent_permission_resolved",
];
/// Only for daemons with the `agentBackgroundWork` feature: the daemon rejects a subscription
/// that names an event it does not know.
const BACKGROUND_WORK_EVENT: &str = "agent.background_work.update";

#[derive(Clone, Debug)]
pub enum ConnectionStatus {
    NotConfigured,
    InvalidEndpoint(SharedString),
    Connecting(Endpoint),
    Connected(Arc<ServerInfo>),
    Disconnected(SharedString),
    /// The daemon has a password and refused what was sent (nothing, the local credential or
    /// a password); the client waits for [`PaseoStore::submit_password`].
    PasswordRequired {
        /// A password was sent and refused, rather than none.
        rejected: bool,
    },
}

/// For open agent sessions and terminals, which follow their own subscriptions.
#[derive(Clone, Debug)]
pub enum PaseoStoreEvent {
    Connected,
    Disconnected,
    AgentStream(AgentStream),
    AgentTimelineReplacement(AgentTimelineReplacement),
    TerminalStreamExit(TerminalStreamExit),
    /// The usage reports, or whether they are being read, changed.
    UsageChanged,
}

/// The live directory of one Paseo daemon: projects, workspaces and agents, kept current through
/// subscriptions that are re-established after every reconnect.
pub struct PaseoStore {
    config: DaemonConfig,
    client: Option<Client>,
    status: ConnectionStatus,
    endpoint: Option<Endpoint>,
    /// Whether the credentials in use are a password, as opposed to the local credential or
    /// none; tells a refused password apart from a missing one.
    sends_password: bool,
    /// A password typed into the prompt, saved once the daemon accepts it.
    typed_password: Option<String>,
    sync_error: Option<SharedString>,
    projects: BTreeMap<String, ProjectSummary>,
    workspaces: HashMap<String, Workspace>,
    agents: HashMap<String, Agent>,
    providers: Vec<ProviderEntry>,
    providers_task: Option<Task<()>>,
    /// The usage of the AI subscriptions the daemon's machine is logged into, as last read; kept
    /// while the daemon is away.
    usage: Vec<UsageReportEntry>,
    usage_error: Option<SharedString>,
    reading_usage: bool,
    usage_task: Option<Task<()>>,
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
    /// The Paseo workspace last opened from the sidebar, per directory. Several Paseo workspaces
    /// can share a directory, and so one Zed workspace.
    selected_workspaces: HashMap<PathBuf, String>,
    /// Agents, terminals, workspaces and projects closed from pazedo, until the daemon's update
    /// removes them; their tabs and rows must not come back in the meantime.
    closing: HashSet<String>,
    /// Repositories being cloned, shown in the sidebar until the clone ends.
    cloning: Vec<SharedString>,
    /// The folder the last clone went into, offered first for the next one.
    last_clone_parent: Option<String>,
    /// Agents whose attention pazedo cleared, with the attention timestamp it cleared, so that
    /// re-rendering a viewed agent before the daemon's update arrives does not clear it again.
    cleared_attention: HashMap<String, Option<String>>,
    /// Background work by agent id, for agents whose list was loaded or pushed since connecting.
    background_work: HashMap<String, Vec<BackgroundWorkItem>>,
    /// Agents whose background work was requested since connecting, so a failed request is not
    /// repeated on every render.
    requested_background_work: HashSet<String>,
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
    /// The same on every daemon for one repository (from its git remote), so the sidebar can
    /// show a repository checked out on several machines as one project.
    pub project_key: Option<String>,
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
    pub fn new(config: DaemonConfig, cx: &mut Context<Self>) -> Self {
        let endpoint = config.endpoint.clone();
        let mut this = Self {
            config,
            client: None,
            status: ConnectionStatus::NotConfigured,
            endpoint: None,
            sends_password: false,
            typed_password: None,
            sync_error: None,
            projects: BTreeMap::default(),
            workspaces: HashMap::default(),
            agents: HashMap::default(),
            providers: Vec::new(),
            providers_task: None,
            usage: Vec::new(),
            usage_error: None,
            reading_usage: false,
            usage_task: None,
            subscriptions: DirectorySubscriptions::default(),
            queued_messages: None,
            sync_task: None,
            terminal_subscriptions: HashMap::default(),
            terminals: HashMap::default(),
            terminal_tasks: HashMap::default(),
            selected_workspaces: HashMap::default(),
            closing: HashSet::default(),
            cloning: Vec::new(),
            last_clone_parent: None,
            cleared_attention: HashMap::default(),
            background_work: HashMap::default(),
            requested_background_work: HashSet::default(),
            _tasks: Vec::new(),
        };
        this.connect(endpoint, cx);
        this
    }

    fn set_config(&mut self, config: DaemonConfig, cx: &mut Context<Self>) {
        if config == self.config {
            return;
        }
        let reconnect = config.endpoint != self.config.endpoint;
        self.config = config;
        if reconnect {
            self.connect(self.config.endpoint.clone(), cx);
        } else {
            // A new `ssh` changes which folders match this daemon's workspaces.
            cx.notify();
        }
    }

    /// The endpoint as configured in settings.json.
    pub fn endpoint_setting(&self) -> Option<&str> {
        self.config.endpoint.as_deref()
    }

    /// The name shown for this daemon: the configured one, else the daemon's host name, else
    /// its endpoint.
    pub fn display_name(&self) -> SharedString {
        if let Some(name) = &self.config.name {
            return name.clone().into();
        }
        if let ConnectionStatus::Connected(server_info) = &self.status {
            return server_info
                .hostname
                .clone()
                .unwrap_or_else(|| server_info.server_id.clone())
                .into();
        }
        self.config.endpoint.clone().unwrap_or_default().into()
    }

    /// The SSH host the daemon's folders are on, or `None` for a daemon on this machine.
    pub fn host(&self, cx: &App) -> Result<Option<RemoteConnectionOptions>> {
        let Some(ssh) = &self.config.ssh else {
            return Ok(None);
        };
        let mut options = SshConnectionOptions::parse_command_line(ssh)
            .with_context(|| format!("invalid paseo ssh setting {ssh:?}"))?;
        RemoteSettings::get_global(cx).fill_connection_options_from_settings(&mut options);
        Ok(Some(RemoteConnectionOptions::Ssh(options)))
    }

    /// Whether the project is on the machine of this daemon's folders: remote on the same SSH
    /// host for a remote daemon, local otherwise. A local folder must not show the agents of a
    /// remote workspace that happens to have the same path, and the other way round.
    pub fn is_on_host(&self, project: &Project, cx: &App) -> bool {
        let project_host = project.remote_connection_options(cx);
        match self.host(cx) {
            Ok(host) => {
                project_host.map(RemoteConnectionOptions::host_key)
                    == host.as_ref().map(RemoteConnectionOptions::host_key)
            }
            Err(_) => false,
        }
    }

    /// Connects to `endpoint`, dropping the connection to any other daemon and everything known
    /// about it.
    fn connect(&mut self, endpoint: Option<String>, cx: &mut Context<Self>) {
        let endpoint = endpoint.filter(|endpoint| !endpoint.trim().is_empty());
        if self.client.take().is_some() {
            // Open agent sessions and terminals treat this like a lost connection.
            cx.emit(PaseoStoreEvent::Disconnected);
        }
        // Dropping the driver task closes the old connection.
        self._tasks.clear();
        self.status = ConnectionStatus::NotConfigured;
        self.endpoint = None;
        self.sends_password = false;
        self.typed_password = None;
        self.sync_error = None;
        self.projects.clear();
        self.workspaces.clear();
        self.agents.clear();
        self.providers.clear();
        self.providers_task = None;
        self.usage.clear();
        self.usage_error = None;
        self.reading_usage = false;
        self.usage_task = None;
        self.subscriptions = DirectorySubscriptions::default();
        self.queued_messages = None;
        self.sync_task = None;
        self.terminal_subscriptions.clear();
        self.terminals.clear();
        self.terminal_tasks.clear();
        self.selected_workspaces.clear();
        self.closing.clear();
        self.cleared_attention.clear();
        self.background_work.clear();
        self.requested_background_work.clear();
        cx.notify();

        let Some(endpoint) = endpoint else {
            return;
        };
        let password_url = daemon_auth::password_url(&endpoint);
        let endpoint = match Endpoint::parse(&endpoint) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.status = ConnectionStatus::InvalidEndpoint(format!("{error:#}").into());
                return;
            }
        };
        self.endpoint = Some(endpoint.clone());
        self.status = ConnectionStatus::Connecting(endpoint.clone());
        // The saved password comes from the keychain, asynchronously; connecting without it
        // first would show a password prompt for a moment.
        self._tasks.push(cx.spawn(async move |this, cx| {
            let auth = daemon_auth::initial_auth(endpoint.clone(), password_url, cx).await;
            this.update(cx, |this, cx| this.start_client(endpoint, auth, cx))
                .ok();
        }));
    }

    fn start_client(
        &mut self,
        endpoint: Endpoint,
        auth: Option<HelloAuth>,
        cx: &mut Context<Self>,
    ) {
        let executor = cx.background_executor().clone();
        let sleep: Sleep = Arc::new(move |duration| executor.timer(duration).boxed());
        let mut options = ConnectOptions::new(endpoint, CLIENT_ID, sleep);
        options.app_version = Some(format!("pazedo {}", env!("CARGO_PKG_VERSION")));
        self.sends_password = matches!(auth, Some(HelloAuth::Password { .. }));
        options.auth = auth;
        let (client, mut events, driver) = Client::new(options);
        self._tasks.push(cx.background_spawn(driver));
        self._tasks.push(cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                if this
                    .update(cx, |this, cx| this.handle_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
        self.client = Some(client);
        cx.notify();
    }

    /// Tries the password; it is saved once the daemon accepts it.
    pub fn submit_password(&mut self, password: String, cx: &mut Context<Self>) {
        let (Some(client), Some(endpoint)) = (self.client.clone(), self.endpoint.clone()) else {
            return;
        };
        client.set_auth(Some(HelloAuth::Password {
            password: password.clone(),
        }));
        self.sends_password = true;
        self.typed_password = Some(password);
        self.status = ConnectionStatus::Connecting(endpoint);
        client.reconnect_now();
        cx.notify();
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

    /// Whether the daemon reports agents' background work (Paseo 0.16 and later).
    pub fn reports_background_work(&self) -> bool {
        matches!(&self.status, ConnectionStatus::Connected(server_info)
            if server_info.has_feature("agentBackgroundWork"))
    }

    pub fn stops_background_work(&self) -> bool {
        matches!(&self.status, ConnectionStatus::Connected(server_info)
            if server_info.has_feature("agentBackgroundWorkStop"))
    }

    /// The agent's background work, or `None` while it is not loaded.
    pub fn background_work(&self, agent_id: &str) -> Option<&[BackgroundWorkItem]> {
        self.background_work.get(agent_id).map(Vec::as_slice)
    }

    /// From the loaded list, else from the count on the agent's snapshot.
    pub fn background_work_count(&self, agent_id: &str) -> usize {
        match self.background_work.get(agent_id) {
            Some(items) => items.len(),
            None => self
                .agents
                .get(agent_id)
                .and_then(|agent| agent.background_work)
                .map_or(0, |work| work.count),
        }
    }

    /// Loads the agent's background work once per connection; `agent.background_work.update`
    /// events keep it current from then on.
    pub fn load_background_work(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        if !self.reports_background_work()
            || self.background_work.contains_key(agent_id)
            || self.requested_background_work.contains(agent_id)
        {
            return;
        }
        let Some(client) = self.client.clone().filter(Client::is_connected) else {
            return;
        };
        self.requested_background_work.insert(agent_id.to_string());
        let agent_id = agent_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = client
                .request(&ListBackgroundWork {
                    agent_id: agent_id.clone(),
                })
                .await
                .and_then(|response| match response.error {
                    Some(error) => Err(anyhow!(error)),
                    None => Ok(response.items),
                });
            this.update(cx, |this, cx| match result {
                Ok(items) => {
                    // An update that arrived meanwhile is newer.
                    this.background_work.entry(agent_id).or_insert(items);
                    cx.notify();
                }
                Err(error) => {
                    log::error!(
                        "failed to list the background work of Paseo agent {agent_id}: {error:#}"
                    )
                }
            })
            .ok();
        })
        .detach();
    }

    /// Stops one background item; the agent's next `agent.background_work.update` removes it.
    pub fn stop_background_work(
        &self,
        agent_id: &str,
        item: &BackgroundWorkItem,
        cx: &App,
    ) -> Task<Result<()>> {
        let Some(client) = self.client.clone().filter(Client::is_connected) else {
            return Task::ready(Err(anyhow!("Not connected to Paseo")));
        };
        let Some(stop_token) = item.stop_token.clone() else {
            return Task::ready(Err(anyhow!("Paseo cannot stop this")));
        };
        let request = StopBackgroundWork {
            agent_id: agent_id.to_string(),
            item_id: item.id.clone(),
            stop_token,
        };
        cx.background_spawn(async move {
            if let Some(error) = client.request(&request).await?.error {
                bail!("Paseo could not stop it: {error}");
            }
            Ok(())
        })
    }

    /// Whether the daemon reports usage (`usage.list_reports`, Paseo 0.14 and later).
    pub fn reports_usage(&self) -> bool {
        matches!(&self.status, ConnectionStatus::Connected(server_info)
            if server_info.has_feature("usageSources"))
    }

    pub fn usage(&self) -> &[UsageReportEntry] {
        &self.usage
    }

    pub fn usage_error(&self) -> Option<&SharedString> {
        self.usage_error.as_ref()
    }

    pub fn is_reading_usage(&self) -> bool {
        self.reading_usage
    }

    /// Reads the usage now, and again every five minutes while connected, as the Paseo app
    /// does. The daemon caches each report for five minutes; `force` skips its cache.
    pub fn read_usage(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone().filter(Client::is_connected) else {
            return;
        };
        if !self.reports_usage() {
            return;
        }
        self.reading_usage = true;
        cx.emit(PaseoStoreEvent::UsageChanged);
        self.usage_task = Some(cx.spawn(async move |this, cx| {
            let mut force = force;
            loop {
                let request = ListUsageReports {
                    force_refresh: force.then_some(true),
                };
                force = false;
                let result = client.request_with_updates(&request).await;
                let updated = this.update(cx, |this, cx| {
                    this.reading_usage = false;
                    match result {
                        Ok((response, updates)) => {
                            this.usage = updates.into_iter().map(|update| update.report).collect();
                            // The daemon streams the reports as they come in; a fixed order keeps
                            // the title bar from shuffling between reads.
                            this.usage.sort_by(|a, b| {
                                (&a.source_label, &a.account.label)
                                    .cmp(&(&b.source_label, &b.account.label))
                            });
                            this.usage_error = response.error.map(SharedString::from);
                        }
                        // Keeps the last reports, as the Paseo app does.
                        Err(error) => {
                            log::warn!("failed to read the Paseo usage: {error:#}");
                            this.usage_error = Some(format!("{error:#}").into());
                        }
                    }
                    cx.emit(PaseoStoreEvent::UsageChanged);
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
                cx.background_executor().timer(USAGE_INTERVAL).await;
            }
        }));
    }

    /// The Paseo workspace a directory stands for: the one last opened from the sidebar, or the
    /// newest one for that directory.
    pub fn workspace_for_directory(&self, directory: &Path) -> Option<&Workspace> {
        self.selected_workspaces
            .get(directory)
            .and_then(|id| self.workspaces.get(id))
            .filter(|workspace| Path::new(workspace.directory()) == directory)
            .or_else(|| {
                self.workspaces
                    .values()
                    .filter(|workspace| Path::new(workspace.directory()) == directory)
                    .max_by(|a, b| a.created_at.cmp(&b.created_at))
            })
    }

    pub fn select_workspace(&mut self, workspace_id: &str, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspaces.get(workspace_id) else {
            return;
        };
        self.selected_workspaces.insert(
            PathBuf::from(workspace.directory()),
            workspace_id.to_string(),
        );
        cx.notify();
    }

    /// The workspace's agents that are neither archived nor being closed, oldest first, which
    /// is the order of their tabs.
    pub fn workspace_agents(&self, workspace_id: &str) -> Vec<&Agent> {
        let mut agents: Vec<&Agent> = self
            .agents
            .values()
            .filter(|agent| {
                agent.workspace_id.as_deref() == Some(workspace_id)
                    && agent.archived_at.is_none()
                    && !self.closing.contains(&agent.id)
            })
            .collect();
        agents.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        agents
    }

    /// The workspace's terminals that are not being closed, empty until they are watched.
    pub fn workspace_terminals(&self, workspace_id: &str) -> Vec<&TerminalInfo> {
        self.terminals
            .get(workspace_id)
            .into_iter()
            .flatten()
            .filter(|terminal| !self.closing.contains(&terminal.id))
            .collect()
    }

    pub fn is_closing(&self, id: &str) -> bool {
        self.closing.contains(id)
    }

    fn connected_client(&self) -> Result<Client> {
        self.client
            .clone()
            .filter(Client::is_connected)
            .context("not connected to a Paseo daemon")
    }

    /// Adds a folder as a Paseo project. The project shows up without workspaces until one is
    /// created in it.
    pub fn add_project(&mut self, path: PathBuf, cx: &mut Context<Self>) -> Task<Result<()>> {
        let client = match self.connected_client() {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.spawn(async move |this, cx| {
            let response = client
                .request(&AddProject {
                    cwd: path.to_string_lossy().to_string(),
                })
                .await?;
            let project = response.project.with_context(|| {
                format!(
                    "Paseo did not add {}: {}",
                    path.display(),
                    response.error.as_deref().unwrap_or("no reason given")
                )
            })?;
            // The `project.update` push may arrive later than the response.
            this.update(cx, |this, cx| {
                this.projects.insert(project.project_id.clone(), project);
                cx.notify();
            })
        })
    }

    /// Removes a project from Paseo. The daemon archives its workspaces (their agents and
    /// terminals) but leaves its files, worktrees included. The project is hidden until the daemon
    /// answers.
    pub fn remove_project(
        &mut self,
        project_id: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let client = match self.connected_client() {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        self.closing.insert(project_id.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = client
                .request(&RemoveProject {
                    project_id: project_id.clone(),
                })
                .await
                .and_then(|response| match response.error {
                    Some(error) => anyhow::bail!("{error}"),
                    None if !response.accepted => anyhow::bail!("Paseo did not remove the project"),
                    None => Ok(response),
                });
            this.update(cx, |this, cx| {
                this.closing.remove(&project_id);
                // The daemon's updates may arrive later than the response.
                if let Ok(response) = &result {
                    this.projects.remove(&project_id);
                    for workspace_id in &response.removed_workspace_ids {
                        this.workspaces.remove(workspace_id);
                    }
                }
                cx.notify();
            })?;
            result.map(|_| ())
        })
    }

    /// The GitHub repositories matching `query` that the daemon machine's `gh` can see.
    pub fn search_github_repositories(
        &self,
        query: String,
        cx: &App,
    ) -> Task<Result<Vec<GithubRepository>>> {
        let client = match self.connected_client() {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.background_spawn(async move {
            let response = client
                .request(&SearchGithubRepositories {
                    query,
                    limit: Some(30),
                })
                .await?;
            if response.status != "success" {
                anyhow::bail!(
                    "{}",
                    response
                        .error
                        .unwrap_or_else(|| format!("GitHub search is {}", response.status))
                );
            }
            Ok(response.repositories)
        })
    }

    /// Clones a repository into `parent_directory` on the daemon's machine and adds it as a
    /// project. `name` labels the clone while it runs.
    pub fn clone_github_project(
        &mut self,
        repo: String,
        clone_protocol: Option<CloneProtocol>,
        parent_directory: String,
        name: SharedString,
        cx: &mut Context<Self>,
    ) -> Task<Result<ProjectSummary>> {
        let client = match self.connected_client() {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        self.cloning.push(name.clone());
        self.last_clone_parent = Some(parent_directory.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = client
                .request(&CloneGithubProject {
                    repo: repo.clone(),
                    clone_protocol,
                    target_directory: parent_directory,
                })
                .await
                .and_then(|response| {
                    response.project.with_context(|| {
                        format!(
                            "Paseo did not clone {repo}: {}",
                            response.error.as_deref().unwrap_or("no reason given")
                        )
                    })
                });
            this.update(cx, |this, cx| {
                if let Some(index) = this.cloning.iter().position(|cloning| *cloning == name) {
                    this.cloning.remove(index);
                }
                if let Ok(project) = &result {
                    this.projects
                        .insert(project.project_id.clone(), project.clone());
                }
                cx.notify();
            })?;
            result
        })
    }

    pub fn cloning(&self) -> &[SharedString] {
        &self.cloning
    }

    /// Where a clone may go: the folder of the last one, then the folders holding the most
    /// projects.
    pub fn clone_parent_suggestions(&self) -> Vec<String> {
        let mut counts: HashMap<String, usize> = HashMap::default();
        for project in self.projects.values() {
            if let Some(parent) = Path::new(&project.project_root_path).parent() {
                *counts
                    .entry(parent.to_string_lossy().into_owned())
                    .or_default() += 1;
            }
        }
        let mut parents: Vec<(String, usize)> = counts.into_iter().collect();
        parents.sort_by(|(a, a_count), (b, b_count)| b_count.cmp(a_count).then_with(|| a.cmp(b)));
        let mut suggestions: Vec<String> = self.last_clone_parent.iter().cloned().collect();
        for (parent, _) in parents {
            if !suggestions.contains(&parent) {
                suggestions.push(parent);
            }
        }
        suggestions
    }

    /// Creates a workspace in a project, from its checkout or as a new worktree.
    pub fn create_workspace(
        &mut self,
        source: WorkspaceSource,
        cx: &mut Context<Self>,
    ) -> Task<Result<Workspace>> {
        let client = match self.connected_client() {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.spawn(async move |this, cx| {
            let response = client
                .request(&CreateWorkspace {
                    source,
                    title: None,
                })
                .await?;
            let workspace = response.workspace.with_context(|| {
                format!(
                    "Paseo did not create the workspace: {}",
                    response.error.as_deref().unwrap_or("no reason given")
                )
            })?;
            this.update(cx, |this, cx| {
                this.workspaces
                    .insert(workspace.id.clone(), workspace.clone());
                cx.notify();
            })?;
            Ok(workspace)
        })
    }

    /// The repository's branches at `cwd`, most recently committed first.
    pub fn branch_suggestions(&self, cwd: String, cx: &App) -> Task<Result<Vec<String>>> {
        let client = match self.connected_client() {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.background_spawn(async move {
            let response = client
                .request(&BranchSuggestions {
                    cwd,
                    query: None,
                    limit: Some(200),
                })
                .await?;
            if let Some(error) = response.error {
                anyhow::bail!("could not list branches: {error}");
            }
            Ok(response.branches)
        })
    }

    /// Folders on the daemon's machine matching a typed path or name, for picking a project on
    /// a remote daemon.
    pub fn directory_suggestions(&self, query: String, cx: &App) -> Task<Result<Vec<String>>> {
        let client = match self.connected_client() {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.background_spawn(async move {
            let response = client
                .request(&DirectorySuggestions {
                    query,
                    include_directories: true,
                    include_files: false,
                    limit: Some(50),
                })
                .await?;
            if let Some(error) = response.error {
                anyhow::bail!("could not list folders: {error}");
            }
            Ok(response.directories)
        })
    }

    /// Resumes a closed agent's provider session, which also rebuilds its timeline.
    pub fn refresh_agent(&self, agent_id: String, cx: &App) -> Task<Result<()>> {
        let client = match self.connected_client() {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.background_spawn(async move {
            client.request(&RefreshAgent { agent_id }).await?;
            Ok(())
        })
    }

    /// Whether the agent finished or failed since the user last saw it. A pending permission
    /// is not cleared by looking at the agent, as in Paseo's apps: it lasts until answered.
    pub fn has_unseen_attention(&self, agent_id: &str) -> bool {
        self.agents.get(agent_id).is_some_and(|agent| {
            agent.requires_attention
                && agent.attention_reason != Some(AttentionReason::Permission)
                && self.cleared_attention.get(agent_id) != Some(&agent.attention_timestamp)
        })
    }

    /// Marks what the agent did as seen, which makes its workspace read (`done`) once no other
    /// agent there needs attention.
    pub fn clear_agent_attention(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        if !self.has_unseen_attention(agent_id) {
            return;
        }
        let Some(client) = self.client.clone().filter(Client::is_connected) else {
            return;
        };
        let attention_timestamp = self
            .agents
            .get(agent_id)
            .and_then(|agent| agent.attention_timestamp.clone());
        self.cleared_attention
            .insert(agent_id.to_string(), attention_timestamp);
        let agent_id = agent_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = client
                .request(&ClearAgentAttention {
                    agent_id: agent_id.clone(),
                })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(response) => {
                    for agent in response.agents {
                        this.agents.insert(agent.id.clone(), agent);
                    }
                    cx.notify();
                }
                Err(error) => {
                    log::error!("failed to mark Paseo agent {agent_id} as seen: {error:#}");
                    this.cleared_attention.remove(&agent_id);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Renames a workspace; `None` or an empty title reverts to the name Paseo derives from its
    /// branch or directory.
    pub fn rename_agent(
        &mut self,
        agent_id: String,
        name: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let client = match self.connected_client() {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.spawn(async move |this, cx| {
            let response = client
                .request(&RenameAgent {
                    agent_id: agent_id.clone(),
                    name: name.clone(),
                })
                .await?;
            if !response.accepted {
                anyhow::bail!(
                    "Paseo did not rename the agent: {}",
                    response.error.as_deref().unwrap_or("no reason given")
                );
            }
            // The daemon sends no update for a closed agent, so the name is applied here.
            this.update(cx, |this, cx| {
                if let Some(agent) = this.agents.get_mut(&agent_id) {
                    agent.title = Some(name);
                    cx.notify();
                }
            })
        })
    }

    pub fn rename_terminal(
        &mut self,
        terminal_id: String,
        title: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let client = match self.connected_client() {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.spawn(async move |this, cx| {
            let response = client
                .request(&RenameTerminal {
                    terminal_id: terminal_id.clone(),
                    title: title.clone(),
                })
                .await?;
            if !response.success {
                anyhow::bail!(
                    "Paseo did not rename the terminal: {}",
                    response.error.as_deref().unwrap_or("no reason given")
                );
            }
            // Shown at once rather than with the next terminal list update.
            this.update(cx, |this, cx| {
                for terminal in this.terminals.values_mut().flatten() {
                    if terminal.id == terminal_id {
                        terminal.title = Some(title.clone());
                        cx.notify();
                    }
                }
            })
        })
    }

    pub fn set_workspace_title(
        &mut self,
        workspace_id: String,
        title: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let client = match self.connected_client() {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.spawn(async move |this, cx| {
            let response = client
                .request(&SetWorkspaceTitle {
                    workspace_id: workspace_id.clone(),
                    title,
                })
                .await?;
            if !response.accepted {
                anyhow::bail!(
                    "Paseo did not rename the workspace: {}",
                    response.error.as_deref().unwrap_or("no reason given")
                );
            }
            // Show the new name without waiting for the workspace update. A reverted name is
            // derived by the daemon, so that one waits.
            if let Some(title) = response.title {
                this.update(cx, |this, cx| {
                    if let Some(workspace) = this.workspaces.get_mut(&workspace_id) {
                        workspace.name = title.clone();
                        workspace.title = Some(title);
                        cx.notify();
                    }
                })?;
            }
            Ok(())
        })
    }

    /// Archives a workspace: its agents, its terminals, and a Paseo worktree that only it uses,
    /// which the daemon deletes from disk. The row is hidden until the daemon answers.
    pub fn archive_workspace(
        &mut self,
        workspace_id: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let client = match self.connected_client() {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        self.closing.insert(workspace_id.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = client
                .request(&ArchiveWorkspace {
                    workspace_id: workspace_id.clone(),
                })
                .await
                .and_then(|response| {
                    if let Some(error) = response.error {
                        anyhow::bail!("{error}");
                    }
                    Ok(response)
                });
            let archived = matches!(&result, Ok(response) if !response.canceled);
            if !archived {
                this.update(cx, |this, cx| {
                    this.closing.remove(&workspace_id);
                    cx.notify();
                })?;
            }
            result.map(|_| ())
        })
    }

    /// Archives agents and kills terminals, as closing their tabs in Paseo's apps does.
    pub fn close_items(
        &mut self,
        agent_ids: Vec<String>,
        terminal_ids: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.closing.extend(agent_ids.iter().cloned());
        self.closing.extend(terminal_ids.iter().cloned());
        cx.notify();
        let request = CloseItems {
            agent_ids,
            terminal_ids,
        };
        cx.spawn(async move |this, cx| {
            if let Err(error) = client.request(&request).await {
                log::error!("failed to close Paseo agents and terminals: {error:#}");
                // Show their tabs again rather than hiding what is still running.
                this.update(cx, |this, cx| {
                    for id in request.agent_ids.iter().chain(&request.terminal_ids) {
                        this.closing.remove(id);
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
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
            .filter(|project| !self.closing.contains(&project.project_id))
            .map(|project| {
                (
                    project.project_id.as_str(),
                    ProjectGroup {
                        project_id: project.project_id.clone(),
                        project_key: project.project_key.clone(),
                        name: project.project_display_name.clone().into(),
                        root_path: project.project_root_path.clone().into(),
                        workspaces: Vec::new(),
                    },
                )
            })
            .collect();

        // A workspace being archived disappears at once, as in Paseo's apps.
        let listed = self.workspaces.values().filter(|workspace| {
            workspace.archiving_at.is_none()
                && !self.closing.contains(&workspace.id)
                && !self.closing.contains(&workspace.project_id)
        });
        for workspace in listed {
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
                    project_key: None,
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
                if let Some(password) = self.typed_password.take()
                    && let Some(endpoint) = self.config.endpoint.as_deref()
                {
                    daemon_auth::save_password(daemon_auth::password_url(endpoint), password, cx)
                        .detach();
                }
                self.sync(cx);
                self.refresh_providers(cx);
                self.read_usage(false, cx);
                cx.emit(PaseoStoreEvent::Connected);
            }
            Event::Disconnected { reason } => {
                // Keep the last known directory on screen; the next sync replaces it.
                self.status = ConnectionStatus::Disconnected(reason.into());
                self.subscriptions = DirectorySubscriptions::default();
                self.queued_messages = None;
                self.sync_task = None;
                self.usage_task = None;
                self.reading_usage = false;
                self.terminal_tasks.clear();
                for subscription in self.terminal_subscriptions.values_mut() {
                    *subscription = None;
                }
                // No updates arrive while disconnected, so the lists may be stale.
                self.background_work.clear();
                self.requested_background_work.clear();
                cx.emit(PaseoStoreEvent::Disconnected);
            }
            Event::Rejected(_) => {
                self.typed_password = None;
                self.status = ConnectionStatus::PasswordRequired {
                    rejected: self.sends_password,
                };
                cx.notify();
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
        let background_work = self.reports_background_work();
        self.sync_task = Some(cx.spawn(async move |this, cx| {
            let result = fetch_directory(&client, background_work).await;
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
            ServerMessage::BackgroundWorkUpdate(update) => {
                self.background_work.insert(update.agent_id, update.items);
            }
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
    /// As an `agent.background_work.update` would set it; the mock provider has none.
    pub fn set_background_work(
        &mut self,
        agent_id: &str,
        items: Vec<BackgroundWorkItem>,
        cx: &mut Context<Self>,
    ) {
        self.background_work.insert(agent_id.to_string(), items);
        cx.notify();
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

async fn fetch_directory(client: &Client, background_work: bool) -> Result<DirectorySnapshot> {
    // Subscribe to project events first so a project created during the fetch is not missed.
    client
        .request(&SetSessionEventSubscription {
            events: SESSION_EVENTS
                .iter()
                .chain(background_work.then_some(&BACKGROUND_WORK_EVENT))
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
