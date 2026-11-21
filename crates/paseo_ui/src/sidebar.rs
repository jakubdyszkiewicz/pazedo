use agent_settings::{AgentSettings, THREADS_LIST_MAX_WIDTH, THREADS_LIST_MIN_WIDTH};
use collections::{HashMap, HashSet};
use db::kvp::KeyValueStore;
use editor::Editor;
use gpui::{
    App, Context, Decorations, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    PathPromptOptions, Pixels, Render, ScrollHandle, SharedString, Subscription, Task, WeakEntity,
    Window, px,
};
use paseo_client::protocol::{
    CheckStatus, ChecksStatus, Mergeable, PullRequest, ReviewDecision, Workspace, WorkspaceKind,
    WorkspaceSource, WorkspaceStatus, WorktreeAction,
};
use platform_title_bar::{SidebarWindowControls, TitleBarSettings, apply_title_bar_insets};
use settings::Settings as _;
use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use theme::{ActiveTheme, CLIENT_SIDE_DECORATION_ROUNDING};
use type_ahead::TypeAhead;
use ui::{
    AgentThreadStatus, CommonAnimationExt, ContextMenu, ContextMenuEntry, IconPosition, Indicator,
    PopoverMenu, PopoverMenuHandle, Tab, ThreadItem, ThreadItemWorktreeInfo, Tooltip, WorktreeKind,
    prelude::*, right_click_menu, utils::platform_title_bar_height,
};
use util::ResultExt as _;
use util::path_list::PathList;
use workspace::{
    CloseWindow, FocusWorkspaceSidebar, MultiWorkspace, MultiWorkspaceEvent, OpenMode, Sidebar,
    SidebarEvent, SidebarSide, notifications::DetachAndPromptErr as _,
};

use crate::{
    add_project::{open_add_project_picker, open_clone_project_picker},
    daemon_auth::PasswordPrompt,
    new_workspace::{NewWorkspaceChoice, open_new_workspace_picker},
    panel::PaseoPanel,
    store::{ConnectionStatus, PaseoDaemons, PaseoStore, ProjectGroup, WorkspaceEntry},
};

/// How the sidebar groups workspaces, as Paseo's app offers: by project, or by what needs the
/// user (Needs input, Failed, Ready to review, Working, Done), across daemons either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Grouping {
    #[default]
    Project,
    Status,
}

const GROUPING_KEY: &str = "pazedo_sidebar_grouping";

gpui::actions!(
    paseo_sidebar,
    [
        /// Starts a new workspace in the project of the selected row (or of the open workspace),
        /// like the project's + button.
        NewWorkspace,
        /// Switches the sidebar between grouping workspaces by project and by status.
        ToggleGrouping,
        /// Shows the open workspace in the sidebar: opens and focuses the sidebar, expands the
        /// workspace's project or status section and selects its row, like the project panel's
        /// reveal of the open file.
        RevealActiveWorkspace,
        /// Collapses every project, or every status section when grouping by status.
        CollapseAll,
        /// Expands every project, or every status section when grouping by status.
        ExpandAll
    ]
);

/// Lets [`RevealActiveWorkspace`] work from anywhere in a window, not only in the sidebar.
pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut workspace::Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &RevealActiveWorkspace, window, cx| {
            let Some(multi_workspace) = workspace.multi_workspace().and_then(WeakEntity::upgrade)
            else {
                return;
            };
            // Opening the sidebar updates every workspace, this one included.
            window.defer(cx, move |window, cx| {
                multi_workspace.update(cx, |multi_workspace, cx| {
                    let sidebar_is_focused = multi_workspace.sidebar_open()
                        && multi_workspace.sidebar().is_some_and(|sidebar| {
                            sidebar.focus_handle(cx).contains_focused(window, cx)
                        });
                    // Opens and focuses it, remembering the focus for escape to go back to.
                    if !sidebar_is_focused {
                        multi_workspace.focus_sidebar(window, cx);
                    }
                });
                let sidebar = multi_workspace
                    .read(cx)
                    .sidebar()
                    .and_then(|sidebar| sidebar.to_any().downcast::<PaseoSidebar>().ok());
                if let Some(sidebar) = sidebar {
                    sidebar.update(cx, |sidebar, cx| sidebar.reveal_active_workspace(cx));
                }
            });
        });
    })
    .detach();
}

enum RenameTarget {
    Workspace(Entity<PaseoStore>, String),
}

/// Where a row came from, shown when rows of several projects and daemons are mixed.
struct RowOrigin {
    branch: Option<SharedString>,
    /// Unset when the rows are already under their project.
    project: Option<SharedString>,
    host: Option<SharedString>,
}

/// A row the keyboard can select.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SidebarEntry {
    /// A project's header, keyed by its first daemon's project.
    Project(DaemonItemKey),
    StatusSection(WorkspaceStatus),
    Workspace(DaemonItemKey),
}

struct SelectableRow {
    entry: SidebarEntry,
    /// The project or status section the row is listed under.
    parent: Option<SidebarEntry>,
    /// Its position among the scrolled list's children.
    index: usize,
    /// What typing in the sidebar matches: a workspace's title.
    search_text: Option<SharedString>,
}

/// The sidebar's list as flat rows (headers and workspaces side by side rather than nested), so
/// that `ScrollHandle::scroll_to_item` can bring the selected row into view.
#[derive(Default)]
struct SidebarRows {
    elements: Vec<AnyElement>,
    selectable: Vec<SelectableRow>,
    /// The workspaces of collapsed projects and status sections, with the one they are under.
    hidden: HashMap<DaemonItemKey, SidebarEntry>,
}

impl SidebarRows {
    fn push(&mut self, element: AnyElement) {
        self.elements.push(element);
    }

    fn push_selectable(
        &mut self,
        entry: SidebarEntry,
        parent: Option<SidebarEntry>,
        element: AnyElement,
    ) {
        self.selectable.push(SelectableRow {
            entry,
            parent,
            index: self.elements.len(),
            search_text: None,
        });
        self.elements.push(element);
    }

    fn push_workspace(
        &mut self,
        entry: SidebarEntry,
        parent: SidebarEntry,
        title: SharedString,
        element: AnyElement,
    ) {
        self.push_selectable(entry, Some(parent), element);
        if let Some(row) = self.selectable.last_mut() {
            row.search_text = Some(title);
        }
    }
}

/// A project as the sidebar lists it when grouping by project. A repository on several daemons
/// is one project, as in Paseo's app: daemons give it the same `projectKey` (from its git
/// remote). A daemon with two projects of one key keeps them apart, as Paseo's app does.
struct MergedProject<D> {
    name: SharedString,
    /// In the order of the daemons.
    placements: Vec<(D, ProjectGroup)>,
}

fn merge_projects<D: Clone + Eq + std::hash::Hash>(
    placements: Vec<(D, ProjectGroup)>,
) -> Vec<MergedProject<D>> {
    let mut key_counts: collections::HashMap<(D, String), usize> = collections::HashMap::default();
    for (daemon, group) in &placements {
        if let Some(key) = &group.project_key {
            *key_counts.entry((daemon.clone(), key.clone())).or_default() += 1;
        }
    }
    let mut projects: Vec<MergedProject<D>> = Vec::new();
    let mut index_by_key: collections::HashMap<String, usize> = collections::HashMap::default();
    for (daemon, group) in placements {
        let shared_key = group
            .project_key
            .clone()
            .filter(|key| key_counts.get(&(daemon.clone(), key.clone())) == Some(&1));
        if let Some(key) = shared_key {
            if let Some(&index) = index_by_key.get(&key) {
                if let Some(project) = projects.get_mut(index) {
                    project.placements.push((daemon, group));
                    continue;
                }
            }
            index_by_key.insert(key, projects.len());
        }
        projects.push(MergedProject {
            name: group.name.clone(),
            placements: vec![(daemon, group)],
        });
    }
    // Stable, so one name on several daemons (not merged) keeps the daemons' order.
    projects.sort_by_key(|project| project.name.to_lowercase());
    projects
}

/// A workspace or project of one daemon. Ids are only unique per daemon: two machines with
/// the same folder can have the same project id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct DaemonItemKey {
    daemon: EntityId,
    id: String,
}

impl DaemonItemKey {
    fn new(store: &Entity<PaseoStore>, id: &str) -> Self {
        Self {
            daemon: store.entity_id(),
            id: id.to_string(),
        }
    }

    fn element_id(&self, prefix: &str) -> SharedString {
        format!("{prefix}-{}-{}", self.daemon, self.id).into()
    }
}

/// Lists the workspaces of the configured Paseo daemons, grouped by project or status, in place
/// of Zed's threads sidebar. Selecting a workspace opens its directory, on the daemon's
/// machine, as the window's active Zed workspace.
pub struct PaseoSidebar {
    multi_workspace: WeakEntity<MultiWorkspace>,
    daemons: Entity<PaseoDaemons>,
    focus_handle: FocusHandle,
    width: Pixels,
    grouping: Grouping,
    collapsed_projects: HashSet<DaemonItemKey>,
    collapsed_buckets: HashSet<WorkspaceStatus>,
    hovered_workspace: Option<DaemonItemKey>,
    /// Several Paseo workspaces can share a directory; this remembers which one was clicked so
    /// only it is highlighted.
    selected_workspace: Option<DaemonItemKey>,
    /// The row chosen with the keyboard (or last clicked), outlined while the sidebar has focus.
    selected_entry: Option<SidebarEntry>,
    /// Whether the selection is shown: set while rendering, from the sidebar's focus.
    shows_selection: bool,
    /// The rows the keyboard can select, in order, as last rendered.
    selectable_rows: Vec<SelectableRow>,
    /// Letters typed in the focused sidebar, which select the best-matching workspace.
    type_ahead: TypeAhead,
    /// The workspaces of collapsed projects and status sections, as last rendered, with the one
    /// they are under.
    hidden_workspaces: HashMap<DaemonItemKey, SidebarEntry>,
    /// Scroll to the selected row once rendered: its section was just expanded.
    scroll_to_selection: bool,
    /// The machine menus of projects on several daemons, so the keyboard can open them too.
    new_workspace_menus: std::cell::RefCell<HashMap<DaemonItemKey, PopoverMenuHandle<ContextMenu>>>,
    rename_editor: Entity<Editor>,
    /// What is being renamed in place.
    renaming: Option<RenameTarget>,
    scroll_handle: ScrollHandle,
    _store_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl PaseoSidebar {
    pub fn new(
        multi_workspace: Entity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let daemons = PaseoDaemons::global(cx);
        let rename_editor = cx.new(|cx| Editor::single_line(window, cx));
        let subscriptions = vec![
            cx.subscribe_in(
                &rename_editor,
                window,
                |this, _, event: &editor::EditorEvent, window, cx| {
                    if let editor::EditorEvent::Blurred = event {
                        this.finish_rename(true, window, cx);
                    }
                },
            ),
            cx.observe(&daemons, |this, _, cx| {
                this.observe_stores(cx);
                cx.notify();
            }),
            cx.subscribe(&multi_workspace, |_, _, event: &MultiWorkspaceEvent, cx| {
                if let MultiWorkspaceEvent::ActiveWorkspaceChanged { .. } = event {
                    cx.notify();
                }
            }),
        ];
        let mut this = Self {
            multi_workspace: multi_workspace.downgrade(),
            daemons,
            focus_handle: cx.focus_handle(),
            width: AgentSettings::get_global(cx).threads_sidebar.default_width,
            grouping: KeyValueStore::global(cx)
                .read_kvp(GROUPING_KEY)
                .log_err()
                .flatten()
                .and_then(|json| serde_json::from_str(&json).log_err())
                .unwrap_or_default(),
            collapsed_projects: HashSet::default(),
            collapsed_buckets: HashSet::default(),
            hovered_workspace: None,
            selected_workspace: None,
            selected_entry: None,
            shows_selection: false,
            selectable_rows: Vec::new(),
            type_ahead: TypeAhead::default(),
            hidden_workspaces: HashMap::default(),
            scroll_to_selection: false,
            new_workspace_menus: Default::default(),
            rename_editor,
            renaming: None,
            scroll_handle: ScrollHandle::new(),
            _store_subscriptions: Vec::new(),
            _subscriptions: subscriptions,
        };
        this.observe_stores(cx);
        this
    }

    #[cfg(test)]
    pub(crate) fn selected_row(&self) -> Option<String> {
        Some(match self.selected_entry.as_ref()? {
            SidebarEntry::Project(key) => format!("project {}", key.id),
            SidebarEntry::StatusSection(bucket) => format!("section {bucket:?}"),
            SidebarEntry::Workspace(key) => format!("workspace {}", key.id),
        })
    }

    fn is_selected(&self, entry: &SidebarEntry) -> bool {
        self.shows_selection && self.selected_entry.as_ref() == Some(entry)
    }

    fn toggle_project(&mut self, project_key: &DaemonItemKey, cx: &mut Context<Self>) {
        if !self.collapsed_projects.remove(project_key) {
            self.collapsed_projects.insert(project_key.clone());
        }
        cx.notify();
    }

    fn toggle_status_section(&mut self, bucket: WorkspaceStatus, cx: &mut Context<Self>) {
        if !self.collapsed_buckets.remove(&bucket) {
            self.collapsed_buckets.insert(bucket);
        }
        cx.notify();
    }

    fn is_collapsed(&self, entry: &SidebarEntry) -> bool {
        match entry {
            SidebarEntry::Project(key) => self.collapsed_projects.contains(key),
            SidebarEntry::StatusSection(bucket) => self.collapsed_buckets.contains(bucket),
            SidebarEntry::Workspace(_) => false,
        }
    }

    fn selected_position(&self) -> Option<usize> {
        let selected = self.selected_entry.as_ref()?;
        self.selectable_rows
            .iter()
            .position(|row| &row.entry == selected)
    }

    fn select_position(&mut self, position: usize, cx: &mut Context<Self>) {
        let Some(row) = self.selectable_rows.get(position) else {
            return;
        };
        self.selected_entry = Some(row.entry.clone());
        self.scroll_handle.scroll_to_item(row.index);
        cx.notify();
    }

    fn select_entry(&mut self, entry: &SidebarEntry, cx: &mut Context<Self>) {
        if let Some(position) = self
            .selectable_rows
            .iter()
            .position(|row| &row.entry == entry)
        {
            self.select_position(position, cx);
        }
    }

    fn select_next(&mut self, _: &menu::SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.type_ahead.end();
        let next = match self.selected_position() {
            Some(position) => position + 1,
            None => 0,
        };
        if next < self.selectable_rows.len() {
            self.select_position(next, cx);
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.type_ahead.end();
        let previous = match self.selected_position() {
            Some(position) => position.saturating_sub(1),
            None => self.selectable_rows.len().saturating_sub(1),
        };
        self.select_position(previous, cx);
    }

    fn select_first(
        &mut self,
        _: &menu::SelectFirst,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.type_ahead.end();
        self.select_position(0, cx);
    }

    fn select_last(&mut self, _: &menu::SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        self.type_ahead.end();
        self.select_position(self.selectable_rows.len().saturating_sub(1), cx);
    }

    /// Opens the selected workspace, or collapses or expands the selected header.
    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.type_ahead.end();
        match self.selected_entry.clone() {
            Some(SidebarEntry::Workspace(key)) => {
                let store = self
                    .stores(cx)
                    .into_iter()
                    .find(|store| store.entity_id() == key.daemon);
                if let Some(store) = store {
                    self.open_workspace(&store, &key.id, window, cx);
                }
            }
            Some(SidebarEntry::Project(key)) => self.toggle_project(&key, cx),
            Some(SidebarEntry::StatusSection(bucket)) => self.toggle_status_section(bucket, cx),
            None => {}
        }
    }

    /// Collapses the selected header, or moves from a workspace to its header.
    fn select_parent(
        &mut self,
        _: &menu::SelectParent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.type_ahead.end();
        let Some(position) = self.selected_position() else {
            return;
        };
        let Some(row) = self.selectable_rows.get(position) else {
            return;
        };
        match (row.entry.clone(), row.parent.clone()) {
            (SidebarEntry::Workspace(_), Some(parent)) => self.select_entry(&parent, cx),
            (SidebarEntry::Project(key), _) if !self.collapsed_projects.contains(&key) => {
                self.toggle_project(&key, cx)
            }
            (SidebarEntry::StatusSection(bucket), _)
                if !self.collapsed_buckets.contains(&bucket) =>
            {
                self.toggle_status_section(bucket, cx)
            }
            _ => {}
        }
    }

    /// Expands the selected header, or moves into an expanded one.
    fn select_child(
        &mut self,
        _: &menu::SelectChild,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.type_ahead.end();
        let Some(position) = self.selected_position() else {
            return;
        };
        let Some(entry) = self
            .selectable_rows
            .get(position)
            .map(|row| row.entry.clone())
        else {
            return;
        };
        if matches!(entry, SidebarEntry::Workspace(_)) {
            return;
        }
        if self.is_collapsed(&entry) {
            match entry {
                SidebarEntry::Project(key) => self.toggle_project(&key, cx),
                SidebarEntry::StatusSection(bucket) => self.toggle_status_section(bucket, cx),
                SidebarEntry::Workspace(_) => {}
            }
        } else if self
            .selectable_rows
            .get(position + 1)
            .is_some_and(|next| next.parent.as_ref() == Some(&entry))
        {
            self.select_position(position + 1, cx);
        }
    }

    /// The project's + button, from the keyboard: for the selected workspace's project, the
    /// selected project, or else the open workspace's project.
    fn new_workspace_for_selection(
        &mut self,
        _: &NewWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let stores = self.stores(cx);
        let workspace_key = match self.selected_entry.clone() {
            Some(SidebarEntry::Workspace(key)) => Some(key),
            Some(SidebarEntry::Project(project_key)) => {
                let placements = stores
                    .iter()
                    .flat_map(|store| {
                        store
                            .read(cx)
                            .project_groups()
                            .into_iter()
                            .map(move |group| (store.clone(), group))
                    })
                    .collect();
                let Some(project) = merge_projects(placements).into_iter().find(|project| {
                    project.placements.first().is_some_and(|(store, group)| {
                        DaemonItemKey::new(store, &group.project_id) == project_key
                    })
                }) else {
                    return;
                };
                if let [(store, group)] = project.placements.as_slice() {
                    self.new_workspace(
                        store.clone(),
                        group.project_id.clone(),
                        project.name.clone(),
                        group.root_path.to_string(),
                        window,
                        cx,
                    );
                } else if let Some(menu) = self.new_workspace_menus.borrow().get(&project_key) {
                    let menu = menu.clone();
                    // The menu reads the sidebar, which is being updated here.
                    window.defer(cx, move |window, cx| menu.toggle(window, cx));
                }
                return;
            }
            Some(SidebarEntry::StatusSection(_)) | None => self
                .highlighted_workspace(cx)
                .map(|(store, id)| DaemonItemKey::new(&store, &id)),
        };
        let Some(key) = workspace_key else {
            return;
        };
        let Some(store) = stores
            .into_iter()
            .find(|store| store.entity_id() == key.daemon)
        else {
            return;
        };
        let Some(workspace) = store.read(cx).workspace(&key.id).cloned() else {
            return;
        };
        self.new_workspace(
            store,
            workspace.project_id,
            workspace.project_display_name.into(),
            workspace.project_root_path,
            window,
            cx,
        );
    }

    /// Typing in the focused sidebar searches the workspaces' titles and selects the best match,
    /// without hiding the others; backspace takes a letter back.
    fn type_ahead_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only the list itself: not the rename editor or an open menu.
        if !self.focus_handle.is_focused(window) || self.renaming.is_some() {
            return;
        }
        if !self.type_ahead.handle_key(&event.keystroke) {
            return;
        }
        cx.stop_propagation();
        let best = self.type_ahead.best_match(
            self.selectable_rows
                .iter()
                .map(|row| row.search_text.as_deref()),
        );
        if let Some(position) = best {
            self.select_position(position, cx);
        }
        cx.notify();
    }

    fn render_type_ahead(&self, cx: &App) -> Option<AnyElement> {
        let has_match = self
            .type_ahead
            .best_match(
                self.selectable_rows
                    .iter()
                    .map(|row| row.search_text.as_deref()),
            )
            .is_some();
        self.type_ahead.render(has_match, "Enter to open", cx)
    }

    /// Hands focus back to where it was before the sidebar took it.
    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        // Escape first ends a search, like it closes a menu.
        if self.type_ahead.end() {
            cx.notify();
            return;
        }
        window.dispatch_action(Box::new(FocusWorkspaceSidebar), cx);
    }

    /// Collapses every project or status section listed. A selected workspace hands the
    /// selection to the header it was under, so it stays visible.
    fn collapse_all(&mut self, _: &CollapseAll, _window: &mut Window, cx: &mut Context<Self>) {
        self.type_ahead.end();
        if let Some(row) = self
            .selected_position()
            .and_then(|position| self.selectable_rows.get(position))
        {
            self.selected_entry = Some(row.parent.clone().unwrap_or_else(|| row.entry.clone()));
            self.scroll_to_selection = true;
        }
        for row in &self.selectable_rows {
            match &row.entry {
                SidebarEntry::Project(project) => {
                    self.collapsed_projects.insert(project.clone());
                }
                SidebarEntry::StatusSection(bucket) => {
                    self.collapsed_buckets.insert(*bucket);
                }
                SidebarEntry::Workspace(_) => {}
            }
        }
        cx.notify();
    }

    /// Expands every project or status section listed, keeping the selection in view.
    fn expand_all(&mut self, _: &ExpandAll, _window: &mut Window, cx: &mut Context<Self>) {
        self.type_ahead.end();
        for row in &self.selectable_rows {
            match &row.entry {
                SidebarEntry::Project(project) => {
                    self.collapsed_projects.remove(project);
                }
                SidebarEntry::StatusSection(bucket) => {
                    self.collapsed_buckets.remove(bucket);
                }
                SidebarEntry::Workspace(_) => {}
            }
        }
        self.scroll_to_selection = true;
        cx.notify();
    }

    /// Selects the open workspace's row, expanding the project or status section it is under,
    /// and scrolls to it.
    fn reveal_active_workspace(&mut self, cx: &mut Context<Self>) {
        self.type_ahead.end();
        let Some((store, workspace_id)) = self.highlighted_workspace(cx) else {
            return;
        };
        let key = DaemonItemKey::new(&store, &workspace_id);
        match self.hidden_workspaces.get(&key) {
            Some(SidebarEntry::Project(project)) => {
                self.collapsed_projects.remove(project);
            }
            Some(SidebarEntry::StatusSection(bucket)) => {
                self.collapsed_buckets.remove(bucket);
            }
            Some(SidebarEntry::Workspace(_)) | None => {}
        }
        self.selected_entry = Some(SidebarEntry::Workspace(key));
        self.scroll_to_selection = true;
        cx.notify();
    }

    fn toggle_grouping(
        &mut self,
        _: &ToggleGrouping,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let grouping = match self.grouping {
            Grouping::Project => Grouping::Status,
            Grouping::Status => Grouping::Project,
        };
        self.set_grouping(grouping, cx);
    }

    #[cfg(test)]
    pub(crate) fn grouping(&self) -> Grouping {
        self.grouping
    }

    pub fn set_grouping(&mut self, grouping: Grouping, cx: &mut Context<Self>) {
        if self.grouping == grouping {
            return;
        }
        self.grouping = grouping;
        cx.notify();
        let Some(json) = serde_json::to_string(&grouping).log_err() else {
            return;
        };
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            kvp.write_kvp(GROUPING_KEY.to_string(), json)
                .await
                .log_err();
        })
        .detach();
    }

    fn render_grouping_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let sidebar = cx.weak_entity();
        let grouping = self.grouping;
        PopoverMenu::new("paseo-grouping-menu")
            .trigger_with_tooltip(
                IconButton::new("paseo-grouping", IconName::ListTree).icon_size(IconSize::Small),
                Tooltip::for_action_title_in(
                    "Group Workspaces",
                    &ToggleGrouping,
                    &self.focus_handle,
                ),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let sidebar = sidebar.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    let entry = |menu: ContextMenu, label: &'static str, choice: Grouping| {
                        let sidebar = sidebar.clone();
                        menu.toggleable_entry(
                            label,
                            grouping == choice,
                            IconPosition::End,
                            None,
                            move |_, cx| {
                                sidebar
                                    .update(cx, |sidebar, cx| sidebar.set_grouping(choice, cx))
                                    .ok();
                            },
                        )
                    };
                    let menu = menu.header("Group By");
                    let menu = entry(menu, "Project", Grouping::Project);
                    entry(menu, "Status", Grouping::Status)
                }))
            })
            .into_any_element()
    }

    fn observe_stores(&mut self, cx: &mut Context<Self>) {
        self._store_subscriptions = self
            .stores(cx)
            .iter()
            .map(|store| cx.observe(store, |_, _, cx| cx.notify()))
            .collect();
    }

    fn stores(&self, cx: &App) -> Vec<Entity<PaseoStore>> {
        self.daemons.read(cx).stores().to_vec()
    }

    /// The workspace row to highlight: among the daemons whose folders are on the machine of
    /// the window's project, the row last clicked if it is still the open directory, otherwise
    /// the first row showing the open directory.
    fn highlighted_workspace(&self, cx: &App) -> Option<(Entity<PaseoStore>, String)> {
        let multi_workspace = self.multi_workspace.upgrade()?;
        let workspace = multi_workspace.read(cx).workspace().read(cx);
        let directory = match workspace.root_paths(cx).as_slice() {
            [path] => path.to_path_buf(),
            _ => return None,
        };
        let project = workspace.project().read(cx);
        self.stores(cx).into_iter().find_map(|store| {
            let store_ref = store.read(cx);
            if !store_ref.is_on_host(project, cx) {
                return None;
            }
            let selected = self
                .selected_workspace
                .as_ref()
                .filter(|selected| selected.daemon == store.entity_id())
                .map(|selected| selected.id.as_str());
            let id = active_workspace_id(&store_ref.project_groups(), &directory, selected)?;
            Some((store.clone(), id))
        })
    }

    #[cfg(test)]
    pub(crate) fn highlighted_workspace_id(&self, cx: &App) -> Option<String> {
        self.highlighted_workspace(cx).map(|(_, id)| id)
    }

    /// Opens the workspace's folder as the window's Zed workspace and shows its agents and
    /// terminals in the Paseo panel.
    pub(crate) fn open_workspace(
        &mut self,
        store: &Entity<PaseoStore>,
        workspace_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(activate) = self.activate_workspace(store, workspace_id, window, cx) else {
            return;
        };
        cx.spawn_in(window, async move |_, cx| {
            let workspace = activate.await?;
            PaseoPanel::reveal_when_ready(&workspace, cx).await
        })
        .detach_and_log_err(cx);
    }

    fn activate_workspace(
        &mut self,
        store: &Entity<PaseoStore>,
        workspace_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<Entity<workspace::Workspace>>>> {
        let directory = PathBuf::from(store.read(cx).workspace(workspace_id)?.directory());
        let host = match store.read(cx).host(cx) {
            Ok(host) => host,
            Err(error) => return Some(Task::ready(Err(error))),
        };
        self.selected_workspace = Some(DaemonItemKey::new(store, workspace_id));
        store.update(cx, |store, cx| store.select_workspace(workspace_id, cx));
        cx.notify();
        let multi_workspace = self.multi_workspace.upgrade()?;
        let modal_workspace = multi_workspace.read(cx).workspace().clone();
        // For a daemon on another machine the folder is there, so it opens over SSH like
        // Zed's remote projects, behind Zed's connection dialog.
        let open = multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.find_or_create_workspace(
                PathList::new(&[directory]),
                host,
                None,
                {
                    let modal_workspace = modal_workspace.clone();
                    move |options, window, cx| {
                        remote_connection::connect_with_modal(&modal_workspace, options, window, cx)
                    }
                },
                None,
                OpenMode::Activate,
                None,
                window,
                cx,
            )
        });
        Some(cx.spawn_in(window, async move |_, cx| {
            let workspace = open.await;
            // The dialog was shown on the workspace that was active, which stays open behind
            // the new remote one.
            remote_connection::dismiss_connection_modal(&modal_workspace, cx);
            workspace
        }))
    }

    fn render_header(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let SidebarWindowControls {
            traffic_lights,
            left: left_window_controls,
            right: right_window_controls,
        } = SidebarWindowControls::new(self.side(cx), window, cx);
        let title_bar_visible = TitleBarSettings::is_title_bar_visible(window, cx);
        let header_height = if title_bar_visible {
            platform_title_bar_height(window)
        } else {
            Tab::container_height(cx)
        };

        let stores = self.stores(cx);

        h_flex()
            .h(header_height)
            .w_full()
            .flex_none()
            .map(|header| {
                if !title_bar_visible {
                    return header.mt_px();
                }
                match window.window_decorations() {
                    Decorations::Client { .. } => apply_title_bar_insets(
                        header,
                        left_window_controls,
                        right_window_controls,
                        false,
                    ),
                    Decorations::Server => header.mt_px().pb_px(),
                }
            })
            .when(left_window_controls, |this| {
                this.children(platform_title_bar::render_left_window_controls(
                    cx.button_layout(),
                    Box::new(CloseWindow),
                    window,
                ))
            })
            .map(|this| {
                if traffic_lights {
                    this.pl(px(ui::utils::TRAFFIC_LIGHT_PADDING))
                } else {
                    this.pl_2()
                }
            })
            .pr_2()
            .gap_1p5()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new("Paseo").size(LabelSize::Small))
            .children(self.render_connection_summary(&stores, cx))
            .child(div().flex_1())
            .children(self.render_add_project_button(&stores, cx))
            .child(self.render_grouping_menu(cx))
            .child(
                IconButton::new("paseo-manage-daemons", IconName::Settings)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Paseo Daemons…"))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_daemons_dialog(window, cx)),
                    ),
            )
            .when(right_window_controls, |this| {
                this.children(platform_title_bar::render_right_window_controls(
                    cx.button_layout(),
                    Box::new(CloseWindow),
                    window,
                ))
            })
            .into_any_element()
    }

    /// How many daemons are connected, e.g. "All online" or "2/3 online", with each daemon's
    /// status on hover. A click opens the Paseo Daemons dialog, which lists them.
    fn render_connection_summary(
        &self,
        stores: &[Entity<PaseoStore>],
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let statuses: Vec<(SharedString, &ConnectionStatus)> = stores
            .iter()
            .map(|store| {
                let store = store.read(cx);
                (store.display_name(), store.status())
            })
            .collect();
        let (color, text) = connection_summary(statuses.iter().map(|(_, status)| *status))?;
        let tooltip: SharedString = statuses
            .iter()
            .map(|(name, status)| format!("{name}: {}", status_label(status)))
            .collect::<Vec<_>>()
            .join("\n")
            .into();
        Some(
            h_flex()
                .id("paseo-connection-summary")
                .min_w_0()
                .gap_1()
                .cursor_pointer()
                .child(Indicator::dot().color(color))
                .child(
                    Label::new(text)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                )
                .tooltip(Tooltip::text(tooltip))
                .on_click(cx.listener(|this, _, window, cx| this.open_daemons_dialog(window, cx)))
                .into_any_element(),
        )
    }

    /// Add Project, on the one daemon, or on the daemon picked from a menu when there are
    /// several. Shown once a daemon is connected.
    /// The header's Add Project menu: open a folder or clone from GitHub, on each daemon when
    /// there are several.
    fn render_add_project_button(
        &self,
        stores: &[Entity<PaseoStore>],
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let is_connected = |store: &Entity<PaseoStore>, cx: &App| {
            matches!(store.read(cx).status(), ConnectionStatus::Connected(_))
        };
        if !stores.iter().any(|store| is_connected(store, cx)) {
            return None;
        }
        let button =
            IconButton::new("paseo-add-project", IconName::Plus).icon_size(IconSize::Small);
        let sidebar = cx.weak_entity();
        let several_daemons = stores.len() > 1;
        let daemons: Vec<_> = stores
            .iter()
            .map(|store| {
                let name = store.read(cx).display_name();
                (store.clone(), name, is_connected(store, cx))
            })
            .collect();
        Some(
            PopoverMenu::new("paseo-add-project-menu")
                .trigger_with_tooltip(button, Tooltip::text("Add Project…"))
                .anchor(gpui::Anchor::TopRight)
                .menu(move |window, cx| {
                    let sidebar = sidebar.clone();
                    let daemons = daemons.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        for (index, (store, name, is_connected)) in daemons.into_iter().enumerate()
                        {
                            if several_daemons {
                                if index > 0 {
                                    menu = menu.separator();
                                }
                                menu = menu.header(format!("On {name}"));
                            }
                            let open_folder = {
                                let sidebar = sidebar.clone();
                                let store = store.clone();
                                move |window: &mut Window, cx: &mut App| {
                                    sidebar
                                        .update(cx, |sidebar, cx| {
                                            sidebar.add_project(&store, window, cx)
                                        })
                                        .ok();
                                }
                            };
                            let clone_repository = {
                                let sidebar = sidebar.clone();
                                move |window: &mut Window, cx: &mut App| {
                                    sidebar
                                        .update(cx, |sidebar, cx| {
                                            sidebar.clone_project(&store, window, cx)
                                        })
                                        .ok();
                                }
                            };
                            menu = menu
                                .item(
                                    ContextMenuEntry::new("Open Folder…")
                                        .icon(IconName::Folder)
                                        .disabled(!is_connected)
                                        .handler(open_folder),
                                )
                                .item(
                                    ContextMenuEntry::new("Clone from GitHub…")
                                        .icon(IconName::Github)
                                        .disabled(!is_connected)
                                        .handler(clone_repository),
                                );
                        }
                        menu
                    }))
                })
                .into_any_element(),
        )
    }

    /// Asks where a new workspace of the project lives (its checkout, or a new worktree off a
    /// branch), creates it and opens it.
    fn new_workspace(
        &mut self,
        store: Entity<PaseoStore>,
        project_id: String,
        project_name: SharedString,
        root_path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let workspace = multi_workspace.read(cx).workspace().clone();
        let sidebar = cx.entity().downgrade();
        workspace.update(cx, |workspace, cx| {
            open_new_workspace_picker(
                workspace,
                &store.clone(),
                project_name,
                root_path.clone(),
                move |choice, window, cx| {
                    let source = match choice {
                        NewWorkspaceChoice::Local => WorkspaceSource::Directory {
                            path: root_path.clone(),
                            project_id: Some(project_id.clone()),
                        },
                        NewWorkspaceChoice::Worktree { base_branch } => WorkspaceSource::Worktree {
                            cwd: root_path.clone(),
                            project_id: Some(project_id.clone()),
                            action: WorktreeAction::BranchOff,
                            ref_name: Some(base_branch),
                            branch_name: None,
                        },
                    };
                    sidebar
                        .update(cx, |sidebar, cx| {
                            sidebar.create_workspace(store.clone(), source, window, cx)
                        })
                        .ok();
                },
                window,
                cx,
            );
        });
    }

    fn create_workspace(
        &mut self,
        store: Entity<PaseoStore>,
        source: WorkspaceSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let create = store.update(cx, |store, cx| store.create_workspace(source, cx));
        cx.spawn_in(window, async move |this, cx| {
            let workspace = create.await?;
            let activate = this.update_in(cx, |this, window, cx| {
                this.activate_workspace(&store, &workspace.id, window, cx)
            })?;
            if let Some(activate) = activate {
                let zed_workspace = activate.await?;
                PaseoPanel::reveal_when_ready(&zed_workspace, cx)
                    .await
                    .log_err();
            }
            // The daemon names a new workspace after its worktree; offer to name it right away.
            // Only now: opening the workspace moves the focus, which would end the rename.
            this.update_in(cx, |this, window, cx| {
                this.selected_entry = Some(SidebarEntry::Workspace(DaemonItemKey::new(
                    &store,
                    &workspace.id,
                )));
                this.start_rename(&store, &workspace.id, window, cx);
            })
        })
        .detach_and_prompt_err(
            "Could not create the workspace",
            window,
            cx,
            |error, _, _| Some(format!("{error:#}")),
        );
    }

    fn start_rename(
        &mut self,
        store: &Entity<PaseoStore>,
        workspace_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = store
            .read(cx)
            .workspace(workspace_id)
            .map(|workspace| workspace.name.clone())
        else {
            return;
        };
        self.begin_rename(
            RenameTarget::Workspace(store.clone(), workspace_id.to_string()),
            name,
            window,
            cx,
        );
    }

    fn begin_rename(
        &mut self,
        target: RenameTarget,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.renaming.is_some() {
            self.finish_rename(true, window, cx);
        }
        self.renaming = Some(target);
        self.rename_editor.update(cx, |editor, cx| {
            editor.set_text(name, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }

    /// Ends renaming in place, saving the edited name unless `save` is false. Like Paseo, an
    /// empty or unchanged name leaves a workspace as it was.
    fn finish_rename(&mut self, save: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.renaming.take() else {
            return;
        };
        cx.notify();
        if self.rename_editor.focus_handle(cx).is_focused(window) {
            self.focus_handle.focus(window, cx);
        }
        if !save {
            return;
        }
        let title = self.rename_editor.read(cx).text(cx).trim().to_string();
        let RenameTarget::Workspace(store, workspace_id) = target;
        let unchanged = store
            .read(cx)
            .workspace(&workspace_id)
            .is_none_or(|workspace| workspace.name == title);
        if title.is_empty() || unchanged {
            return;
        }
        store
            .update(cx, |store, cx| {
                store.set_workspace_title(workspace_id, Some(title), cx)
            })
            .detach_and_prompt_err(
                "Could not rename the workspace",
                window,
                cx,
                |error, _, _| Some(format!("{error:#}")),
            );
    }

    /// Asks for confirmation, then archives the workspace. Archiving cannot be undone from
    /// pazedo and deletes a Paseo worktree from disk, so the question says what goes.
    fn archive_workspace(
        &mut self,
        store: &Entity<PaseoStore>,
        workspace_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = store.read(cx).workspace(workspace_id).cloned() else {
            return;
        };
        let deletes_worktree = workspace.workspace_kind == WorkspaceKind::Worktree
            && workspace
                .git_runtime
                .as_ref()
                .and_then(|git| git.is_paseo_owned_worktree)
                .unwrap_or(false);
        let mut detail = "Its agents are archived and its terminals are closed.".to_string();
        if deletes_worktree {
            detail.push_str(&format!(
                " Its worktree at {} is deleted, with any changes not pushed.",
                workspace.directory()
            ));
        }
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("Archive “{}”?", workspace.name),
            Some(&detail),
            &["Archive", "Cancel"],
            cx,
        );
        let store = store.clone();
        let workspace_id = workspace_id.to_string();
        cx.spawn_in(window, async move |_, cx| {
            if answer.await? != 0 {
                return Ok(());
            }
            store
                .update(cx, |store, cx| store.archive_workspace(workspace_id, cx))
                .await
        })
        .detach_and_prompt_err(
            "Could not archive the workspace",
            window,
            cx,
            |error, _, _| Some(format!("{error:#}")),
        );
    }

    /// Asks for confirmation, then removes the project from each daemon it is on. Its
    /// workspaces are archived with it, so the question says how many.
    fn remove_project(
        &mut self,
        name: &str,
        removals: Vec<(Entity<PaseoStore>, String)>,
        workspace_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut detail = match workspace_count {
            0 => String::new(),
            1 => "Its workspace is archived, with its agents and terminals. ".to_string(),
            count => {
                format!("Its {count} workspaces are archived, with their agents and terminals. ")
            }
        };
        detail.push_str("Files on disk, worktrees included, are not changed.");
        if removals.len() > 1 {
            let daemons = removals
                .iter()
                .map(|(store, _)| store.read(cx).display_name().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            detail.push_str(&format!(" It is removed from {daemons}."));
        }
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("Remove “{name}” from Paseo?"),
            Some(&detail),
            &["Remove", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |_, cx| {
            if answer.await? != 0 {
                return Ok(());
            }
            let removed = cx.update(|_, cx| {
                removals
                    .into_iter()
                    .map(|(store, project_id)| {
                        store.update(cx, |store, cx| store.remove_project(project_id, cx))
                    })
                    .collect::<Vec<_>>()
            })?;
            for result in futures::future::join_all(removed).await {
                result?;
            }
            anyhow::Ok(())
        })
        .detach_and_prompt_err(
            "Could not remove the project",
            window,
            cx,
            |error, _, _| Some(format!("{error:#}")),
        );
    }

    fn render_rename_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .h_full()
            .min_w_0()
            .flex_1()
            .capture_action(
                cx.listener(|this, _: &editor::actions::Newline, window, cx| {
                    this.finish_rename(true, window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.finish_rename(true, window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &editor::actions::Cancel, window, cx| {
                    this.finish_rename(false, window, cx);
                }),
            )
            // Clicks while editing place the cursor rather than open the workspace.
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(self.rename_editor.clone())
            .into_any_element()
    }

    /// Asks for a folder and adds it to Paseo as a project: with the native folder dialog for a
    /// daemon on this machine, or with folders the daemon suggests for one on another machine.
    fn add_project(
        &mut self,
        store: &Entity<PaseoStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = match store.read(cx).host(cx) {
            Ok(host) => host,
            Err(error) => {
                Task::<anyhow::Result<()>>::ready(Err(error)).detach_and_prompt_err(
                    "Could not add the project",
                    window,
                    cx,
                    |error, _, _| Some(format!("{error:#}")),
                );
                return;
            }
        };
        if let Some(host) = host {
            let Some(multi_workspace) = self.multi_workspace.upgrade() else {
                return;
            };
            let workspace = multi_workspace.read(cx).workspace().clone();
            let store = store.clone();
            workspace.update(cx, |workspace, cx| {
                open_add_project_picker(
                    workspace,
                    store.clone(),
                    host.display_name().into(),
                    move |path, window, cx| {
                        store
                            .update(cx, |store, cx| store.add_project(PathBuf::from(path), cx))
                            .detach_and_prompt_err(
                                "Could not add the project",
                                window,
                                cx,
                                |error, _, _| Some(format!("{error:#}")),
                            );
                    },
                    window,
                    cx,
                );
            });
            return;
        }
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add Project".into()),
        });
        let store = store.clone();
        cx.spawn_in(window, async move |_, cx| {
            let Some(path) = paths.await??.and_then(|paths| paths.into_iter().next()) else {
                return Ok(());
            };
            store
                .update(cx, |store, cx| store.add_project(path, cx))
                .await
        })
        .detach_and_prompt_err("Could not add the project", window, cx, |error, _, _| {
            Some(format!("{error:#}"))
        });
    }

    /// Asks for a GitHub repository and a folder on the daemon's machine, clones it there as a
    /// project, then selects the project.
    fn clone_project(
        &mut self,
        store: &Entity<PaseoStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let workspace = multi_workspace.read(cx).workspace().clone();
        let sidebar = cx.weak_entity();
        let store = store.clone();
        let host_name = store.read(cx).display_name();
        workspace.update(cx, |workspace, cx| {
            open_clone_project_picker(
                workspace,
                store.clone(),
                host_name,
                move |source, parent, window, cx| {
                    let cloned = store.update(cx, |store, cx| {
                        store.clone_github_project(
                            source.repo.clone(),
                            source.clone_protocol,
                            parent,
                            source.name_with_owner.clone().into(),
                            cx,
                        )
                    });
                    let sidebar = sidebar.clone();
                    let store = store.clone();
                    window
                        .spawn(cx, async move |cx| {
                            let project = cloned.await?;
                            sidebar.update(cx, |sidebar, cx| {
                                let key = DaemonItemKey::new(&store, &project.project_id);
                                sidebar.collapsed_projects.remove(&key);
                                sidebar.selected_entry = Some(SidebarEntry::Project(key));
                                sidebar.scroll_to_selection = true;
                                cx.notify();
                            })
                        })
                        .detach_and_prompt_err(
                            "Could not clone the repository",
                            window,
                            cx,
                            |error, _, _| Some(format!("{error:#}")),
                        );
                },
                window,
                cx,
            );
        });
    }

    /// A row for each repository the daemon is cloning.
    fn render_clones(
        &self,
        store: &Entity<PaseoStore>,
        several_daemons: bool,
        cx: &App,
    ) -> Vec<AnyElement> {
        let store = store.read(cx);
        store
            .cloning()
            .iter()
            .map(|repository| {
                let message = if several_daemons {
                    format!("Cloning {repository} on {}…", store.display_name())
                } else {
                    format!("Cloning {repository}…")
                };
                h_flex()
                    .flex_none()
                    .gap_1p5()
                    .px_2()
                    .py_1p5()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Icon::new(IconName::ArrowCircle)
                            .size(IconSize::Small)
                            .color(Color::Muted)
                            .with_rotate_animation(2),
                    )
                    .child(
                        Label::new(message)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// The daemon's problem, if any, named after the daemon when there are several.
    fn render_message(
        &self,
        store: &PaseoStore,
        several_daemons: bool,
        cx: &App,
    ) -> Option<AnyElement> {
        let (icon, message): (IconName, SharedString) = match store.status() {
            ConnectionStatus::NotConfigured => (
                IconName::Info,
                "Not connected to Paseo. Set the daemon's address in settings.json, for example \
                 \"paseo\": { \"endpoint\": \"127.0.0.1:6767\" }."
                    .into(),
            ),
            ConnectionStatus::InvalidEndpoint(error) => (IconName::Warning, error.clone()),
            ConnectionStatus::Disconnected(reason) => {
                (IconName::Warning, format!("{reason}. Reconnecting…").into())
            }
            // Rendered with its button by `render_password_notice`.
            ConnectionStatus::PasswordRequired { .. } => return None,
            ConnectionStatus::Connecting(_) | ConnectionStatus::Connected(_) => {
                let error = store.sync_error()?;
                (
                    IconName::Warning,
                    format!("Failed to load workspaces: {error}").into(),
                )
            }
        };
        let message = if several_daemons {
            format!("{}: {message}", store.display_name()).into()
        } else {
            message
        };
        Some(render_notice(icon, message, cx))
    }

    fn render_password_notice(
        &self,
        store: &Entity<PaseoStore>,
        rejected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = store.read(cx).display_name();
        let message: SharedString = if rejected {
            format!("{name} rejected the password.").into()
        } else {
            format!("{name} needs a password.").into()
        };
        v_flex()
            .flex_none()
            .gap_1()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .items_start()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::Lock)
                            .size(IconSize::Small)
                            .color(Color::Warning),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(message)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    ),
            )
            .child(
                h_flex().pl_5().child(
                    Button::new(
                        SharedString::from(format!("paseo-password-{}", store.entity_id())),
                        "Enter Password…",
                    )
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener({
                        let store = store.clone();
                        move |this, _, window, cx| {
                            this.open_password_prompt(store.clone(), rejected, window, cx)
                        }
                    })),
                ),
            )
            .into_any_element()
    }

    fn open_password_prompt(
        &mut self,
        store: Entity<PaseoStore>,
        rejected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let workspace = multi_workspace.read(cx).workspace().clone();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                PasswordPrompt::new(store, rejected, window, cx)
            });
        });
    }

    /// A daemon's problem (not connected, a password needed, an error), above the workspaces.
    fn render_daemon_notice(
        &self,
        store: &Entity<PaseoStore>,
        several_daemons: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        match store.read(cx).status() {
            ConnectionStatus::PasswordRequired { rejected } => {
                Some(self.render_password_notice(store, *rejected, cx))
            }
            _ => self.render_message(store.read(cx), several_daemons, cx),
        }
    }

    fn render_status_groups(
        &self,
        stores: &[Entity<PaseoStore>],
        active_workspace: Option<&(Entity<PaseoStore>, String)>,
        several_daemons: bool,
        rows_out: &mut SidebarRows,
        cx: &mut Context<Self>,
    ) {
        let mut rows: Vec<(Entity<PaseoStore>, SharedString, WorkspaceEntry)> = Vec::new();
        for store in stores {
            let store_ref = store.read(cx);
            if !matches!(store_ref.status(), ConnectionStatus::Connected(_)) {
                continue;
            }
            for group in store_ref.project_groups() {
                for entry in group.workspaces {
                    rows.push((store.clone(), group.name.clone(), entry));
                }
            }
        }
        let mut by_bucket: collections::HashMap<WorkspaceStatus, Vec<_>> =
            collections::HashMap::default();
        for row in rows {
            by_bucket
                .entry(status_bucket(row.2.workspace.status))
                .or_default()
                .push(row);
        }
        let mut is_first = true;
        for bucket in STATUS_BUCKETS {
            let Some(mut bucket_rows) = by_bucket.remove(&bucket) else {
                continue;
            };
            // Most recently changed first, as in Paseo's app.
            bucket_rows.sort_by(|(_, project_a, a), (_, project_b, b)| {
                b.workspace
                    .status_entered_at
                    .cmp(&a.workspace.status_entered_at)
                    .then_with(|| project_a.cmp(project_b))
                    .then_with(|| a.workspace.name.cmp(&b.workspace.name))
            });
            let is_collapsed = self.collapsed_buckets.contains(&bucket);
            let label = bucket_label(bucket);
            let section = SidebarEntry::StatusSection(bucket);
            let is_selected = self.is_selected(&section);
            let colors = cx.theme().colors();
            // The same header as a project's when grouping by project.
            let header = div()
                .w_full()
                .when(!is_first, |this| {
                    this.border_t_1().border_color(colors.border_variant)
                })
                .child(
                    h_flex()
                        .id(SharedString::from(format!("paseo-status-{bucket:?}")))
                        .h(Tab::content_height(cx))
                        .w_full()
                        .px_2()
                        .gap_1()
                        .border_1()
                        .border_color(if is_selected {
                            colors.panel_focused_border
                        } else {
                            gpui::transparent_black()
                        })
                        .cursor_pointer()
                        .hover(|style| style.bg(colors.ghost_element_hover))
                        .active(|style| style.bg(colors.ghost_element_active))
                        .child(
                            Icon::new(if is_collapsed {
                                IconName::ChevronRight
                            } else {
                                IconName::ChevronDown
                            })
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                        )
                        .child(
                            Label::new(label)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        )
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.selected_entry = Some(SidebarEntry::StatusSection(bucket));
                            this.toggle_status_section(bucket, cx);
                        })),
                );
            rows_out.push_selectable(section.clone(), None, header.into_any_element());
            is_first = false;
            if is_collapsed {
                for (store, _, entry) in bucket_rows {
                    rows_out.hidden.insert(
                        DaemonItemKey::new(&store, &entry.workspace.id),
                        section.clone(),
                    );
                }
                continue;
            }
            for (store, project, entry) in bucket_rows {
                let is_active = active_workspace.is_some_and(|(active_store, id)| {
                    *active_store == store && *id == entry.workspace.id
                });
                let origin = RowOrigin {
                    branch: None,
                    project: Some(project),
                    host: several_daemons.then(|| store.read(cx).display_name()),
                };
                let workspace =
                    SidebarEntry::Workspace(DaemonItemKey::new(&store, &entry.workspace.id));
                let title: SharedString = entry.workspace.name.clone().into();
                let row = self.render_workspace(&store, entry, is_active, Some(origin), cx);
                rows_out.push_workspace(workspace, section.clone(), title, row);
            }
        }
    }

    fn render_not_configured(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .child(render_notice(
                IconName::Info,
                "Not connected to Paseo. Add a daemon to see its workspaces.".into(),
                cx,
            ))
            .child(
                h_flex().px_2().py_1p5().child(
                    Button::new("paseo-add-first-daemon", "Add Daemon…")
                        .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_daemons_dialog(window, cx)),
                        ),
                ),
            )
            .into_any_element()
    }

    fn open_daemons_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let workspace = multi_workspace.read(cx).workspace().clone();
        workspace.update(cx, |workspace, cx| {
            crate::daemon_settings::open_daemons_dialog(workspace, window, cx);
        });
    }

    fn render_project(
        &self,
        project: MergedProject<Entity<PaseoStore>>,
        active_workspace: Option<&(Entity<PaseoStore>, String)>,
        show_hosts: bool,
        is_first: bool,
        rows_out: &mut SidebarRows,
        cx: &mut Context<Self>,
    ) {
        let Some((first_store, first_group)) = project.placements.first() else {
            return;
        };
        let project_key = DaemonItemKey::new(first_store, &first_group.project_id);
        let entry = SidebarEntry::Project(project_key.clone());
        let is_selected = self.is_selected(&entry);
        let is_collapsed = self.collapsed_projects.contains(&project_key);
        let mut rows: Vec<(Entity<PaseoStore>, WorkspaceEntry)> = Vec::new();
        let mut placements: Vec<(Entity<PaseoStore>, String, SharedString)> = Vec::new();
        for (store, group) in project.placements {
            placements.push((store.clone(), group.project_id, group.root_path));
            rows.extend(
                group
                    .workspaces
                    .into_iter()
                    .map(|entry| (store.clone(), entry)),
            );
        }
        // The order each daemon uses, across daemons: pinned, then newest first.
        rows.sort_by(|(_, a), (_, b)| {
            let (a, b) = (&a.workspace, &b.workspace);
            Reverse(&a.pinned_at)
                .cmp(&Reverse(&b.pinned_at))
                .then_with(|| Reverse(&a.created_at).cmp(&Reverse(&b.created_at)))
                .then_with(|| a.id.cmp(&b.id))
        });
        let attention_count = rows
            .iter()
            .filter(|(_, entry)| needs_attention(entry.workspace.status))
            .count();
        let tooltip: SharedString = match placements.as_slice() {
            [(_, _, root_path)] => root_path.clone(),
            placements => placements
                .iter()
                .map(|(store, _, root_path)| {
                    format!("{}: {root_path}", store.read(cx).display_name())
                })
                .collect::<Vec<_>>()
                .join("\n")
                .into(),
        };
        let removals: Vec<(Entity<PaseoStore>, String)> = placements
            .iter()
            .map(|(store, project_id, _)| (store.clone(), project_id.clone()))
            .collect();
        let workspace_count = rows.len();
        let new_workspace_button =
            self.render_new_workspace_button(&project_key, &project.name, placements, cx);
        let color = cx.theme().colors();

        let header = h_flex()
            .id(project_key.element_id("paseo-project"))
            .h(Tab::content_height(cx))
            .w_full()
            .px_2()
            .gap_1()
            .border_1()
            .border_color(if is_selected {
                color.panel_focused_border
            } else {
                gpui::transparent_black()
            })
            .cursor_pointer()
            .hover(|style| style.bg(color.ghost_element_hover))
            .active(|style| style.bg(color.ghost_element_active))
            .child(
                Icon::new(if is_collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(
                Label::new(project.name.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            )
            .when(is_collapsed && attention_count > 0, |this| {
                this.child(Indicator::dot().color(Color::Accent))
            })
            .child(div().flex_1())
            .child(new_workspace_button)
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener({
                let project_key = project_key.clone();
                move |this, _, _window, cx| {
                    this.selected_entry = Some(SidebarEntry::Project(project_key.clone()));
                    this.toggle_project(&project_key, cx);
                }
            }));

        let header = header.into_any_element();
        let sidebar = cx.weak_entity();
        let project_name = project.name.clone();
        let header = right_click_menu(project_key.element_id("paseo-project-menu"))
            .trigger(move |_, _, _| header)
            .menu(move |window, cx| {
                let sidebar = sidebar.clone();
                let project_name = project_name.clone();
                let removals = removals.clone();
                ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.entry("Remove Project…", None, move |window, cx| {
                        sidebar
                            .update(cx, |sidebar, cx| {
                                sidebar.remove_project(
                                    &project_name,
                                    removals.clone(),
                                    workspace_count,
                                    window,
                                    cx,
                                )
                            })
                            .ok();
                    })
                })
            });
        let header = div()
            .w_full()
            .when(!is_first, |this| {
                this.border_t_1().border_color(color.border_variant)
            })
            .child(header);
        rows_out.push_selectable(entry.clone(), None, header.into_any_element());
        if is_collapsed {
            for (store, workspace) in rows {
                rows_out.hidden.insert(
                    DaemonItemKey::new(&store, &workspace.workspace.id),
                    entry.clone(),
                );
            }
            return;
        }
        if rows.is_empty() {
            rows_out.push(
                div()
                    .pl_6()
                    .pb_1()
                    .child(
                        Label::new("No workspaces")
                            .size(LabelSize::Small)
                            .color(Color::Disabled),
                    )
                    .into_any_element(),
            );
            return;
        }
        for (store, workspace) in rows {
            let is_active = active_workspace.is_some_and(|(active_store, id)| {
                *active_store == store && *id == workspace.workspace.id
            });
            let is_connected = matches!(store.read(cx).status(), ConnectionStatus::Connected(_));
            let origin = show_hosts.then(|| RowOrigin {
                branch: None,
                project: None,
                host: Some(store.read(cx).display_name()),
            });
            let workspace_entry =
                SidebarEntry::Workspace(DaemonItemKey::new(&store, &workspace.workspace.id));
            let title: SharedString = workspace.workspace.name.clone().into();
            let row = self.render_workspace(&store, workspace, is_active, origin, cx);
            rows_out.push_workspace(
                workspace_entry,
                entry.clone(),
                title,
                div()
                    .w_full()
                    .when(!is_connected, |this| this.opacity(0.6))
                    .child(row)
                    .into_any_element(),
            );
        }
    }

    /// The project's New Workspace button. A project on several daemons first asks which
    /// machine the workspace goes on.
    fn render_new_workspace_button(
        &self,
        project_key: &DaemonItemKey,
        name: &SharedString,
        placements: Vec<(Entity<PaseoStore>, String, SharedString)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let button_id = project_key.element_id("paseo-new-workspace");
        if let [(store, project_id, root_path)] = placements.as_slice() {
            return IconButton::new(button_id, IconName::Plus)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text("New Workspace…"))
                .on_click(cx.listener({
                    let store = store.clone();
                    let project_id = project_id.clone();
                    let name = name.clone();
                    let root_path = root_path.clone();
                    move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.new_workspace(
                            store.clone(),
                            project_id.clone(),
                            name.clone(),
                            root_path.to_string(),
                            window,
                            cx,
                        );
                    }
                }))
                .into_any_element();
        }
        let sidebar = cx.weak_entity();
        let name = name.clone();
        let hosts: Vec<_> = placements
            .into_iter()
            .map(|(store, project_id, root_path)| {
                let host = store.read(cx).display_name();
                (store, project_id, root_path, host)
            })
            .collect();
        // The header toggles the project on click; the menu's button must not.
        div()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                PopoverMenu::new(project_key.element_id("paseo-new-workspace-menu"))
                    .with_handle(
                        self.new_workspace_menus
                            .borrow_mut()
                            .entry(project_key.clone())
                            .or_default()
                            .clone(),
                    )
                    .trigger_with_tooltip(
                        IconButton::new(button_id, IconName::Plus).icon_size(IconSize::Small),
                        Tooltip::text("New Workspace…"),
                    )
                    .anchor(gpui::Anchor::TopRight)
                    .menu(move |window, cx| {
                        let sidebar = sidebar.clone();
                        let name = name.clone();
                        let hosts = hosts.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                            let mut menu = menu.header("New Workspace On");
                            for (store, project_id, root_path, host) in hosts {
                                let sidebar = sidebar.clone();
                                let name = name.clone();
                                menu = menu.entry(host, None, move |window, cx| {
                                    sidebar
                                        .update(cx, |sidebar, cx| {
                                            sidebar.new_workspace(
                                                store.clone(),
                                                project_id.clone(),
                                                name.clone(),
                                                root_path.to_string(),
                                                window,
                                                cx,
                                            )
                                        })
                                        .ok();
                                });
                            }
                            menu
                        }))
                    }),
            )
            .into_any_element()
    }

    fn render_workspace(
        &self,
        store: &Entity<PaseoStore>,
        entry: WorkspaceEntry,
        is_active: bool,
        origin: Option<RowOrigin>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace = &entry.workspace;
        let workspace_id = workspace.id.clone();
        let key = DaemonItemKey::new(store, &workspace.id);
        let is_hovered = self.hovered_workspace.as_ref() == Some(&key);
        let is_renaming = matches!(
            &self.renaming,
            Some(RenameTarget::Workspace(renaming_store, id))
                if renaming_store == store && *id == workspace.id
        );

        let (status, notified) = match workspace.status {
            WorkspaceStatus::Running => (AgentThreadStatus::Running, false),
            WorkspaceStatus::NeedsInput => (AgentThreadStatus::WaitingForConfirmation, false),
            WorkspaceStatus::Failed => (AgentThreadStatus::Error, false),
            WorkspaceStatus::Attention => (AgentThreadStatus::Completed, true),
            WorkspaceStatus::Done | WorkspaceStatus::Unknown => {
                (AgentThreadStatus::Completed, false)
            }
        };
        let is_worktree = workspace.workspace_kind == WorkspaceKind::Worktree;
        let branch = workspace
            .git_runtime
            .as_ref()
            .and_then(|git| git.current_branch.clone());
        // A worktree shows its name, and its branch when that differs (the daemon names new
        // worktrees and their branches alike); a checkout shows its branch. `ThreadItem` only
        // renders linked worktrees, hence the kind for both. Rows that also show their project
        // and daemon keep to the branch, as Paseo's app does.
        let mixes_projects = origin
            .as_ref()
            .is_some_and(|origin| origin.project.is_some());
        let worktree_name = workspace
            .worktree_slug
            .clone()
            .filter(|_| is_worktree && !mixes_projects);
        let branch = branch.or_else(|| worktree_name.clone().filter(|_| mixes_projects));
        let branch_name = branch.filter(|branch| Some(branch) != worktree_name.as_ref());
        // Rows with their project and daemon show the branch among those details instead, so
        // that it gives way before them.
        let mut origin = origin;
        let branch_name = match origin.as_mut() {
            Some(origin) if mixes_projects => {
                origin.branch = branch_name.map(Into::into);
                None
            }
            _ => branch_name,
        };
        let worktrees = if worktree_name.is_some() || branch_name.is_some() {
            vec![ThreadItemWorktreeInfo {
                worktree_name: worktree_name.map(Into::into),
                branch_name: branch_name.map(Into::into),
                full_path: workspace.directory().to_string().into(),
                highlight_positions: Vec::new(),
                kind: WorktreeKind::Linked,
            }]
        } else {
            Vec::new()
        };
        let pull_request = workspace
            .github_runtime
            .as_ref()
            .and_then(|github| github.pull_request.clone());

        let mut item = ThreadItem::new(key.element_id("paseo-workspace"), workspace.name.clone())
            .base_bg(cx.theme().colors().panel_background)
            // Shown when no status icon replaces it: a workspace whose agents' work was seen.
            .icon(IconName::Circle)
            .icon_color(Color::Disabled)
            .status(status)
            .notified(notified)
            .worktrees(worktrees)
            .selected(is_active)
            .focused(self.is_selected(&SidebarEntry::Workspace(key.clone())))
            .hovered(is_hovered)
            .on_click(cx.listener({
                let store = store.clone();
                let workspace_id = workspace_id.clone();
                move |this, _, window, cx| {
                    this.selected_entry = Some(SidebarEntry::Workspace(DaemonItemKey::new(
                        &store,
                        &workspace_id,
                    )));
                    this.open_workspace(&store, &workspace_id, window, cx);
                }
            }));
        if is_renaming {
            item = item
                .is_truncated(false)
                .title_slot(self.render_rename_editor(cx));
        } else if is_hovered {
            item = item.action_slot(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new(key.element_id("paseo-rename-workspace"), IconName::Pencil)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Rename Workspace"))
                            .on_click(cx.listener({
                                let store = store.clone();
                                let workspace_id = workspace_id.clone();
                                move |this, _, window, cx| {
                                    this.start_rename(&store, &workspace_id, window, cx)
                                }
                            })),
                    )
                    .child(
                        IconButton::new(
                            key.element_id("paseo-archive-workspace"),
                            IconName::Archive,
                        )
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Archive Workspace…"))
                        .on_click(cx.listener({
                            let store = store.clone();
                            let workspace_id = workspace_id.clone();
                            move |this, _, window, cx| {
                                this.archive_workspace(&store, &workspace_id, window, cx)
                            }
                        })),
                    ),
            );
        }
        if origin.is_some() || pull_request.is_some() {
            item = item.trailing_metadata(render_row_details(&key, origin, pull_request.as_ref()));
        }

        let agent_count = entry.agents.len();
        let running_agent_count = entry.running_agent_count();
        let pending_permission_count = entry.pending_permission_count();
        let tooltip: SharedString = format!(
            "{}\n{agent_count} agent{}{}{}",
            workspace.directory(),
            if agent_count == 1 { "" } else { "s" },
            if running_agent_count > 0 {
                format!(", {running_agent_count} running")
            } else {
                String::new()
            },
            if pending_permission_count > 0 {
                format!(", {pending_permission_count} waiting for permission")
            } else {
                String::new()
            },
        )
        .into();
        let sidebar = cx.weak_entity();
        let menu = right_click_menu(key.element_id("paseo-workspace-menu"))
            .trigger(move |_, _, _| item)
            .menu({
                let store = store.clone();
                move |window, cx| {
                    let sidebar = sidebar.clone();
                    let store = store.clone();
                    let workspace_id = workspace_id.clone();
                    let pull_request_url = pull_request
                        .as_ref()
                        .map(|pull_request| pull_request.url.clone());
                    ContextMenu::build(window, cx, move |menu, _, _| {
                        menu.entry("Rename Workspace", None, {
                            let sidebar = sidebar.clone();
                            let store = store.clone();
                            let workspace_id = workspace_id.clone();
                            move |window, cx| {
                                sidebar
                                    .update(cx, |sidebar, cx| {
                                        sidebar.start_rename(&store, &workspace_id, window, cx)
                                    })
                                    .ok();
                            }
                        })
                        .when_some(pull_request_url, |menu, url| {
                            menu.entry("Open Pull Request", None, move |_, cx| cx.open_url(&url))
                        })
                        .separator()
                        .entry(
                            "Archive Workspace…",
                            None,
                            move |window, cx| {
                                sidebar
                                    .update(cx, |sidebar, cx| {
                                        sidebar.archive_workspace(&store, &workspace_id, window, cx)
                                    })
                                    .ok();
                            },
                        )
                    })
                }
            });
        div()
            .id(key.element_id("paseo-workspace-row"))
            .w_full()
            .child(menu)
            .on_hover(cx.listener(move |this, is_hovered: &bool, _window, cx| {
                if *is_hovered {
                    this.hovered_workspace = Some(key.clone());
                } else if this.hovered_workspace.as_ref() == Some(&key) {
                    this.hovered_workspace = None;
                }
                cx.notify();
            }))
            .tooltip(Tooltip::text(tooltip))
            .into_any_element()
    }
}

fn render_notice(icon: IconName, message: SharedString, cx: &App) -> AnyElement {
    h_flex()
        .flex_none()
        .items_start()
        .gap_1p5()
        .px_2()
        .py_1p5()
        .border_b_1()
        .border_color(cx.theme().colors().border_variant)
        .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
        .child(
            div().flex_1().min_w_0().child(
                Label::new(message)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            ),
        )
        .into_any_element()
}

/// The header's dot and text for the daemons' statuses: green when all are connected, red when
/// none is and none is on its way, orange otherwise. `None` without daemons.
fn connection_summary<'a>(
    statuses: impl IntoIterator<Item = &'a ConnectionStatus>,
) -> Option<(Color, SharedString)> {
    let statuses: Vec<_> = statuses.into_iter().collect();
    let connected = statuses
        .iter()
        .filter(|status| matches!(status, ConnectionStatus::Connected(_)))
        .count();
    let connecting = statuses.iter().any(|status| {
        matches!(
            status,
            ConnectionStatus::Connecting(_) | ConnectionStatus::PasswordRequired { .. }
        )
    });
    let color = if connected == statuses.len() {
        Color::Success
    } else if connected == 0 && !connecting {
        Color::Error
    } else {
        Color::Warning
    };
    let text: SharedString = match statuses.as_slice() {
        [] => return None,
        [status] => status_label(status).into(),
        _ if connected == statuses.len() => "All online".into(),
        _ => format!("{connected}/{} online", statuses.len()).into(),
    };
    Some((color, text))
}

fn status_label(status: &ConnectionStatus) -> &'static str {
    match status {
        ConnectionStatus::Connected(_) => "Online",
        ConnectionStatus::Connecting(_) => "Connecting…",
        ConnectionStatus::Disconnected(_) => "Offline",
        ConnectionStatus::PasswordRequired { .. } => "Password required",
        ConnectionStatus::NotConfigured | ConnectionStatus::InvalidEndpoint(_) => "Not connected",
    }
}

fn active_workspace_id(
    groups: &[ProjectGroup],
    active_directory: &Path,
    selected: Option<&str>,
) -> Option<String> {
    let matching: Vec<&Workspace> = groups
        .iter()
        .flat_map(|group| &group.workspaces)
        .map(|entry| &entry.workspace)
        .filter(|workspace| Path::new(workspace.directory()) == active_directory)
        .collect();
    matching
        .iter()
        .find(|workspace| Some(workspace.id.as_str()) == selected)
        .or(matching.first())
        .map(|workspace| workspace.id.clone())
}

const STATUS_BUCKETS: [WorkspaceStatus; 5] = [
    WorkspaceStatus::NeedsInput,
    WorkspaceStatus::Failed,
    WorkspaceStatus::Attention,
    WorkspaceStatus::Running,
    WorkspaceStatus::Done,
];

/// Whether a workspace waits for the user: unread work, a failure or a question. A running
/// workspace does not, until its agents stop.
fn needs_attention(status: WorkspaceStatus) -> bool {
    matches!(
        status,
        WorkspaceStatus::Attention | WorkspaceStatus::NeedsInput | WorkspaceStatus::Failed
    )
}

fn status_bucket(status: WorkspaceStatus) -> WorkspaceStatus {
    match status {
        WorkspaceStatus::Unknown => WorkspaceStatus::Done,
        status => status,
    }
}

fn bucket_label(bucket: WorkspaceStatus) -> &'static str {
    match bucket {
        WorkspaceStatus::NeedsInput => "Needs input",
        WorkspaceStatus::Failed => "Failed",
        WorkspaceStatus::Attention => "Ready to review",
        WorkspaceStatus::Running => "Working",
        WorkspaceStatus::Done | WorkspaceStatus::Unknown => "Done",
    }
}

/// The end of a row's second line: its project and daemon when rows of several are mixed,
/// then its pull request.
fn render_row_details(
    key: &DaemonItemKey,
    origin: Option<RowOrigin>,
    pull_request: Option<&PullRequest>,
) -> AnyElement {
    let separator = || {
        Label::new("•")
            .size(LabelSize::Small)
            .color(Color::Disabled)
            .into_any_element()
    };
    // Each detail gives way down to a few characters; the project and daemon are also capped,
    // and the pull request keeps its width.
    let detail = |icon: IconName, text: SharedString, capped: bool| {
        // A minimum wider than a short value would leave a gap after it.
        let short = text.chars().count() <= 6;
        h_flex()
            .gap_0p5()
            .map(|this| match (short, capped) {
                (true, _) => this.flex_none(),
                (false, true) => this.min_w(rems(3.)).max_w(rems(6.)),
                (false, false) => this.min_w(rems(3.5)),
            })
            .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
            .child(
                Label::new(text)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            )
            .into_any_element()
    };
    let mut parts: Vec<AnyElement> = Vec::new();
    if let Some(origin) = origin {
        if let Some(branch) = origin.branch {
            parts.push(detail(IconName::GitBranch, branch, false));
        }
        if let Some(project) = origin.project {
            parts.push(detail(IconName::Folder, project, true));
        }
        if let Some(host) = origin.host {
            parts.push(detail(IconName::Server, host, true));
        }
    }
    if let Some(pull_request) = pull_request {
        parts.push(render_pull_request(key, pull_request));
    }
    let mut row = h_flex().min_w_0().gap_1();
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            row = row.child(separator());
        }
        row = row.child(part);
    }
    row.into_any_element()
}

/// The pull request's number and its CI's state, e.g. "<PR icon> 5 · CI ✓", with the details
/// (title, state, review, each check) on hover.
fn render_pull_request(key: &DaemonItemKey, pull_request: &PullRequest) -> AnyElement {
    let ci = match pull_request.checks_status {
        Some(ChecksStatus::Success) => Some((IconName::Check, Color::Success)),
        Some(ChecksStatus::Failure) => Some((IconName::XCircle, Color::Error)),
        // Not a spinner, which would look like a running agent.
        Some(ChecksStatus::Pending) => Some((IconName::Clock, Color::Warning)),
        Some(ChecksStatus::None | ChecksStatus::Unknown) | None => None,
    };
    let state_color = if pull_request.is_merged {
        Color::Accent
    } else if pull_request.state.eq_ignore_ascii_case("closed") {
        Color::Disabled
    } else {
        Color::Muted
    };
    let tooltip = pull_request_tooltip(pull_request);
    h_flex()
        .id(key.element_id("paseo-pull-request"))
        .flex_none()
        .gap_1()
        .child(
            h_flex()
                .gap_0p5()
                .child(
                    Icon::new(IconName::PullRequest)
                        .size(IconSize::XSmall)
                        .color(state_color),
                )
                .children(pull_request.number.map(|number| {
                    Label::new(number.to_string())
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                })),
        )
        .when_some(ci, |this, (icon, color)| {
            this.child(
                Label::new("•")
                    .size(LabelSize::Small)
                    .color(Color::Disabled),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(Label::new("CI").size(LabelSize::Small).color(Color::Muted))
                    .child(Icon::new(icon).size(IconSize::XSmall).color(color)),
            )
        })
        .tooltip(Tooltip::text(tooltip))
        .into_any_element()
}

fn pull_request_tooltip(pull_request: &PullRequest) -> SharedString {
    let mut lines = vec![match pull_request.number {
        Some(number) => format!("#{number} {}", pull_request.title),
        None => pull_request.title.clone(),
    }];
    let mut state = vec![if pull_request.is_merged {
        "Merged"
    } else if pull_request.state.eq_ignore_ascii_case("closed") {
        "Closed"
    } else if pull_request.is_draft == Some(true) {
        "Draft"
    } else {
        "Open"
    }];
    match pull_request.review_decision {
        Some(ReviewDecision::Approved) => state.push("approved"),
        Some(ReviewDecision::ChangesRequested) => state.push("changes requested"),
        Some(ReviewDecision::Pending) => state.push("review pending"),
        Some(ReviewDecision::Unknown) | None => {}
    }
    if pull_request.mergeable == Some(Mergeable::Conflicting) {
        state.push("has conflicts");
    }
    lines.push(state.join(", "));
    for check in &pull_request.checks {
        let status = match check.status {
            CheckStatus::Success => "✓",
            CheckStatus::Failure => "✗",
            CheckStatus::Pending => "…",
            CheckStatus::Skipped | CheckStatus::Cancelled | CheckStatus::Unknown => "–",
        };
        let mut line = format!("{status} {}", check.name);
        if let Some(duration) = &check.duration {
            line.push_str(&format!(" ({duration})"));
        }
        lines.push(line);
    }
    lines.join("\n").into()
}

impl Sidebar for PaseoSidebar {
    fn width(&self, _cx: &App) -> Pixels {
        self.width
    }

    fn set_width(&mut self, width: Option<Pixels>, cx: &mut Context<Self>) {
        self.width = width
            .unwrap_or_else(|| AgentSettings::get_global(cx).threads_sidebar.default_width)
            .clamp(THREADS_LIST_MIN_WIDTH, THREADS_LIST_MAX_WIDTH);
        cx.notify();
    }

    fn has_notifications(&self, cx: &App) -> bool {
        self.stores(cx).iter().any(|store| {
            store
                .read(cx)
                .project_groups()
                .iter()
                .flat_map(|group| &group.workspaces)
                .any(|entry| needs_attention(entry.workspace.status))
        })
    }

    fn side(&self, cx: &App) -> SidebarSide {
        AgentSettings::get_global(cx).sidebar_side()
    }

    /// Focusing the sidebar from the keyboard starts at the open workspace's row.
    fn prepare_for_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Called while `MultiWorkspace` is being updated, which the open workspace is read from.
        cx.defer_in(window, |this, _window, cx| {
            if let Some((store, workspace_id)) = this.highlighted_workspace(cx) {
                let entry = SidebarEntry::Workspace(DaemonItemKey::new(&store, &workspace_id));
                this.selected_entry = Some(entry.clone());
                this.select_entry(&entry, cx);
            }
            cx.notify();
        });
    }
}

impl EventEmitter<SidebarEvent> for PaseoSidebar {}

impl Focusable for PaseoSidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PaseoSidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui_font = theme_settings::setup_ui_font(window, cx);
        let color = cx.theme().colors();
        let background = color.background.blend(color.panel_background);
        let border = color.border;
        let on_left = self.side(cx) == SidebarSide::Left;

        let active_workspace = self.highlighted_workspace(cx);
        if let Some((store, workspace_id)) = active_workspace.clone()
            && !store.read(cx).is_watching_terminals(&workspace_id)
        {
            cx.defer(move |cx| {
                store.update(cx, |store, cx| store.watch_terminals(&workspace_id, cx));
            });
        }
        self.shows_selection = self.focus_handle.contains_focused(window, cx);
        let header = self.render_header(window, cx);
        let stores = self.stores(cx);
        let several_daemons = stores.len() > 1;
        let mut rows = SidebarRows::default();
        if stores.is_empty() {
            rows.push(self.render_not_configured(cx));
        } else {
            // The daemons' problems, then every daemon's workspaces by status or by project.
            for store in &stores {
                if let Some(notice) = self.render_daemon_notice(store, several_daemons, cx) {
                    rows.push(notice);
                }
                for clone in self.render_clones(store, several_daemons, cx) {
                    rows.push(clone);
                }
            }
            match self.grouping {
                Grouping::Status => self.render_status_groups(
                    &stores,
                    active_workspace.as_ref(),
                    several_daemons,
                    &mut rows,
                    cx,
                ),
                Grouping::Project => {
                    let placements = stores
                        .iter()
                        .flat_map(|store| {
                            store
                                .read(cx)
                                .project_groups()
                                .into_iter()
                                .map(move |group| (store.clone(), group))
                        })
                        .collect();
                    let projects = merge_projects(placements);
                    // As in Paseo's app, rows name their machine once the projects span several.
                    let show_hosts = projects
                        .iter()
                        .flat_map(|project| project.placements.iter().map(|(store, _)| store))
                        .collect::<HashSet<_>>()
                        .len()
                        > 1;
                    for (index, project) in projects.into_iter().enumerate() {
                        self.render_project(
                            project,
                            active_workspace.as_ref(),
                            show_hosts,
                            index == 0,
                            &mut rows,
                            cx,
                        );
                    }
                }
            }
        }
        self.selectable_rows = rows.selectable;
        self.hidden_workspaces = rows.hidden;
        if std::mem::take(&mut self.scroll_to_selection)
            && let Some(row) = self
                .selected_position()
                .and_then(|position| self.selectable_rows.get(position))
        {
            self.scroll_handle.scroll_to_item(row.index);
        }
        let type_ahead = self.render_type_ahead(cx);
        let mut key_context = gpui::KeyContext::new_with_defaults(cx.platform_info().os);
        key_context.add("PaseoSidebar");
        // Gives the sidebar the list keys: up and down, enter, escape (bound globally to `menu`
        // actions) and left and right (bound in the `menu` context).
        key_context.add("menu");

        v_flex()
            .id("paseo-sidebar")
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::select_parent))
            .on_action(cx.listener(Self::select_child))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::new_workspace_for_selection))
            .on_action(cx.listener(Self::toggle_grouping))
            .on_action(cx.listener(Self::collapse_all))
            .on_action(cx.listener(Self::expand_all))
            .on_action(cx.listener(|this, _: &RevealActiveWorkspace, _window, cx| {
                this.reveal_active_workspace(cx)
            }))
            .on_key_down(cx.listener(Self::type_ahead_key))
            .font(ui_font)
            .map(|this| match window.window_decorations() {
                Decorations::Server => this.h_full().w(self.width),
                // Mirrors the threads sidebar: with client-side decorations the sidebar owns the
                // window corners on its side.
                Decorations::Client { tiling, .. } => this
                    .absolute()
                    .top(if tiling.top { px(0.) } else { px(-1.) })
                    .bottom(if tiling.bottom { px(0.) } else { px(-1.) })
                    .when(!tiling.top, |this| this.pt_px())
                    .when(!tiling.bottom, |this| this.pb_px())
                    .map(|this| {
                        if on_left {
                            this.right(px(0.))
                                .left(if tiling.left { px(0.) } else { px(-1.) })
                                .when(!tiling.left, |this| this.pl(px(1.)))
                        } else {
                            this.left(px(0.))
                                .right(if tiling.right { px(0.) } else { px(-1.) })
                                .when(!tiling.right, |this| this.pr(px(1.)))
                        }
                    })
                    .when(on_left && !(tiling.top || tiling.left), |this| {
                        this.rounded_tl(CLIENT_SIDE_DECORATION_ROUNDING)
                    })
                    .when(on_left && !(tiling.bottom || tiling.left), |this| {
                        this.rounded_bl(CLIENT_SIDE_DECORATION_ROUNDING)
                    })
                    .when(!on_left && !(tiling.top || tiling.right), |this| {
                        this.rounded_tr(CLIENT_SIDE_DECORATION_ROUNDING)
                    })
                    .when(!on_left && !(tiling.bottom || tiling.right), |this| {
                        this.rounded_br(CLIENT_SIDE_DECORATION_ROUNDING)
                    }),
            })
            .bg(background)
            .when(on_left, |this| this.border_r_1())
            .when(!on_left, |this| this.border_l_1())
            .border_color(border)
            .child(header)
            .child(
                v_flex()
                    .id("paseo-sidebar-projects")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .children(rows.elements),
            )
            .children(type_ahead)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paseo_client::protocol::ServerInfo;

    fn project(id: &str, key: Option<&str>, name: &str) -> ProjectGroup {
        ProjectGroup {
            project_id: id.to_string(),
            project_key: key.map(str::to_string),
            name: name.to_string().into(),
            root_path: format!("/dev/{name}").into(),
            workspaces: Vec::new(),
        }
    }

    fn summarize(projects: &[MergedProject<&'static str>]) -> Vec<(String, Vec<String>)> {
        projects
            .iter()
            .map(|project| {
                let placements = project
                    .placements
                    .iter()
                    .map(|(daemon, group)| format!("{daemon}/{}", group.project_id))
                    .collect();
                (project.name.to_string(), placements)
            })
            .collect()
    }

    #[test]
    fn only_waiting_workspaces_need_attention() {
        let waiting: Vec<WorkspaceStatus> = STATUS_BUCKETS
            .into_iter()
            .chain([WorkspaceStatus::Unknown])
            .filter(|status| needs_attention(*status))
            .collect();
        assert_eq!(
            waiting,
            [
                WorkspaceStatus::NeedsInput,
                WorkspaceStatus::Failed,
                WorkspaceStatus::Attention,
            ],
            "a running workspace marks nothing until its agents stop"
        );
    }

    #[test]
    fn one_repository_on_several_daemons_is_one_project() {
        let webapp = "remote:github.com/example/webapp";
        let projects = merge_projects(vec![
            ("mini", project("prj_1", Some(webapp), "webapp")),
            (
                "mini",
                project("prj_2", Some("host:mini:/tmp/notes"), "notes"),
            ),
            ("devbox", project("prj_9", Some(webapp), "webapp")),
            ("devbox", project("prj_8", None, "Docs")),
        ]);
        assert_eq!(
            summarize(&projects),
            [
                ("Docs".to_string(), vec!["devbox/prj_8".to_string()]),
                ("notes".to_string(), vec!["mini/prj_2".to_string()]),
                (
                    "webapp".to_string(),
                    vec!["mini/prj_1".to_string(), "devbox/prj_9".to_string()]
                ),
            ]
        );
    }

    #[test]
    fn a_key_shared_within_one_daemon_keeps_its_projects_apart() {
        let key = "remote:github.com/example/monorepo";
        let projects = merge_projects(vec![
            ("mini", project("prj_1", Some(key), "monorepo")),
            ("mini", project("prj_2", Some(key), "monorepo")),
            ("devbox", project("prj_9", Some(key), "monorepo")),
        ]);
        assert_eq!(
            summarize(&projects),
            [
                ("monorepo".to_string(), vec!["mini/prj_1".to_string()]),
                ("monorepo".to_string(), vec!["mini/prj_2".to_string()]),
                ("monorepo".to_string(), vec!["devbox/prj_9".to_string()]),
            ]
        );
    }

    #[test]
    fn summarizes_the_daemons_connections() {
        let connected = ConnectionStatus::Connected(std::sync::Arc::new(ServerInfo {
            protocol_version: None,
            server_id: "mini".into(),
            hostname: None,
            version: None,
            permissions: Vec::new(),
            features: Default::default(),
        }));
        let offline = ConnectionStatus::Disconnected("connection refused".into());
        let password = ConnectionStatus::PasswordRequired { rejected: false };
        let summary = |statuses: &[&ConnectionStatus]| {
            connection_summary(statuses.iter().copied())
                .map(|(color, text)| (color, text.to_string()))
        };

        assert_eq!(summary(&[]), None);
        assert_eq!(
            summary(&[&connected]),
            Some((Color::Success, "Online".into()))
        );
        assert_eq!(summary(&[&offline]), Some((Color::Error, "Offline".into())));
        assert_eq!(
            summary(&[&connected, &connected]),
            Some((Color::Success, "All online".into()))
        );
        assert_eq!(
            summary(&[&connected, &offline, &connected]),
            Some((Color::Warning, "2/3 online".into()))
        );
        assert_eq!(
            summary(&[&offline, &offline]),
            Some((Color::Error, "0/2 online".into()))
        );
        assert_eq!(
            summary(&[&offline, &password]),
            Some((Color::Warning, "0/2 online".into()))
        );
    }

    #[test]
    fn status_groups_follow_paseos_order() {
        assert_eq!(
            STATUS_BUCKETS.map(bucket_label),
            [
                "Needs input",
                "Failed",
                "Ready to review",
                "Working",
                "Done"
            ]
        );
        assert_eq!(
            status_bucket(WorkspaceStatus::Unknown),
            WorkspaceStatus::Done,
            "a status this build does not know is listed with the done ones"
        );
    }

    #[test]
    fn describes_a_pull_request_and_its_checks() {
        let pull_request: PullRequest = serde_json::from_value(serde_json::json!({
            "number": 5,
            "url": "https://github.com/example/webapp/pull/5",
            "title": "Switch web client to a dark theme",
            "state": "open",
            "isMerged": false,
            "isDraft": true,
            "mergeable": "CONFLICTING",
            "reviewDecision": "changes_requested",
            "checksStatus": "failure",
            "checks": [
                {"name": "test", "status": "success", "url": null, "duration": "1m 17s"},
                {"name": "lint", "status": "failure", "url": null},
                {"name": "deploy", "status": "some_new_status", "url": null},
            ],
        }))
        .unwrap();
        assert_eq!(
            pull_request_tooltip(&pull_request),
            "#5 Switch web client to a dark theme\n\
             Draft, changes requested, has conflicts\n\
             ✓ test (1m 17s)\n\
             ✗ lint\n\
             – deploy"
        );
    }
}
