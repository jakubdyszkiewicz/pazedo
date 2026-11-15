use agent_settings::{AgentSettings, THREADS_LIST_MAX_WIDTH, THREADS_LIST_MIN_WIDTH};
use collections::HashSet;
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
use std::path::{Path, PathBuf};
use theme::{ActiveTheme, CLIENT_SIDE_DECORATION_ROUNDING};
use ui::{
    AgentThreadStatus, ContextMenu, IconPosition, Indicator, PopoverMenu, Tab, ThreadItem,
    ThreadItemWorktreeInfo, Tooltip, WorktreeKind, prelude::*, right_click_menu,
    utils::platform_title_bar_height,
};
use util::ResultExt as _;
use util::path_list::PathList;
use workspace::{
    CloseWindow, MultiWorkspace, MultiWorkspaceEvent, OpenMode, Sidebar, SidebarEvent, SidebarSide,
    notifications::DetachAndPromptErr as _,
};

use crate::{
    add_project::open_add_project_picker,
    new_workspace::{NewWorkspaceChoice, open_new_workspace_picker},
    panel::PaseoPanel,
    store::{
        ConnectionStatus, ENDPOINT_ENV_VAR, PaseoDaemons, PaseoStore, ProjectGroup, WorkspaceEntry,
    },
};

/// How the sidebar groups workspaces, as Paseo's app offers: under their daemon and project, or
/// across daemons by what needs the user (Needs input, Failed, Ready to review, Working, Done).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Grouping {
    #[default]
    Project,
    Status,
}

const GROUPING_KEY: &str = "pazedo_sidebar_grouping";

/// Where a row came from, shown when rows of several projects and daemons are mixed.
struct RowOrigin {
    branch: Option<SharedString>,
    project: SharedString,
    host: Option<SharedString>,
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

/// Lists the workspaces of the configured Paseo daemons, grouped by daemon and project, in place
/// of Zed's threads sidebar. Selecting a workspace opens its directory, on the daemon's
/// machine, as the window's active Zed workspace.
pub struct PaseoSidebar {
    multi_workspace: WeakEntity<MultiWorkspace>,
    daemons: Entity<PaseoDaemons>,
    focus_handle: FocusHandle,
    width: Pixels,
    grouping: Grouping,
    collapsed_daemons: HashSet<EntityId>,
    collapsed_projects: HashSet<DaemonItemKey>,
    collapsed_buckets: HashSet<WorkspaceStatus>,
    hovered_workspace: Option<DaemonItemKey>,
    /// Several Paseo workspaces can share a directory; this remembers which one was clicked so
    /// only it is highlighted.
    selected_workspace: Option<DaemonItemKey>,
    rename_editor: Entity<Editor>,
    /// The workspace whose name is being edited in place.
    renaming_workspace: Option<(Entity<PaseoStore>, String)>,
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
            collapsed_daemons: HashSet::default(),
            collapsed_projects: HashSet::default(),
            collapsed_buckets: HashSet::default(),
            hovered_workspace: None,
            selected_workspace: None,
            rename_editor,
            renaming_workspace: None,
            scroll_handle: ScrollHandle::new(),
            _store_subscriptions: Vec::new(),
            _subscriptions: subscriptions,
        };
        this.observe_stores(cx);
        this
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
                Tooltip::text("Group Workspaces"),
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

        // With a single daemon its status and Add Project sit in the header, as before there
        // could be several; with more, each daemon's section has its own.
        let stores = self.stores(cx);
        let single_daemon = match stores.as_slice() {
            [store] => Some(store.clone()),
            _ => None,
        };

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
            .when_some(single_daemon.clone(), |this, store| {
                this.child(self.render_daemon_status(&store, Color::Muted, cx))
            })
            .child(div().flex_1())
            .child(self.render_grouping_menu(cx))
            .when_some(single_daemon, |this, store| {
                this.children(self.render_add_project_button(&store, cx))
            })
            .when(right_window_controls, |this| {
                this.children(platform_title_bar::render_right_window_controls(
                    cx.button_layout(),
                    Box::new(CloseWindow),
                    window,
                ))
            })
            .into_any_element()
    }

    /// The daemon's connection dot and name, with the connection's details on hover.
    fn render_daemon_status(
        &self,
        store: &Entity<PaseoStore>,
        name_color: Color,
        cx: &App,
    ) -> AnyElement {
        let store_ref = store.read(cx);
        let (indicator_color, status_text): (Color, SharedString) = match store_ref.status() {
            ConnectionStatus::Connected(_) => (Color::Success, store_ref.display_name()),
            ConnectionStatus::Connecting(_) => (
                Color::Warning,
                format!("{} · Connecting…", store_ref.display_name()).into(),
            ),
            ConnectionStatus::Disconnected(_) => (
                Color::Error,
                format!("{} · Disconnected", store_ref.display_name()).into(),
            ),
            ConnectionStatus::NotConfigured | ConnectionStatus::InvalidEndpoint(_) => {
                (Color::Muted, "Not connected".into())
            }
        };
        let tooltip = connection_tooltip(store_ref);
        h_flex()
            .id(SharedString::from(format!(
                "paseo-connection-status-{}",
                store.entity_id()
            )))
            .min_w_0()
            .gap_1()
            .child(Indicator::dot().color(indicator_color))
            .child(
                Label::new(status_text)
                    .size(LabelSize::Small)
                    .color(name_color)
                    .truncate(),
            )
            .tooltip(move |_window, cx| Tooltip::simple(tooltip.clone(), cx))
            .into_any_element()
    }

    fn render_add_project_button(
        &self,
        store: &Entity<PaseoStore>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !matches!(store.read(cx).status(), ConnectionStatus::Connected(_)) {
            return None;
        }
        Some(
            IconButton::new(
                SharedString::from(format!("paseo-add-project-{}", store.entity_id())),
                IconName::Plus,
            )
            .icon_size(IconSize::Small)
            .tooltip(Tooltip::text("Add Project…"))
            .on_click(cx.listener({
                let store = store.clone();
                move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.add_project(&store, window, cx)
                }
            }))
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
            this.update_in(cx, |this, window, cx| {
                this.open_workspace(&store, &workspace.id, window, cx);
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
        if self.renaming_workspace.is_some() {
            self.finish_rename(true, window, cx);
        }
        self.renaming_workspace = Some((store.clone(), workspace_id.to_string()));
        self.rename_editor.update(cx, |editor, cx| {
            editor.set_text(name, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }

    /// Ends renaming in place, saving the edited name unless `save` is false. Like Paseo, an
    /// empty or unchanged name leaves the workspace as it was.
    fn finish_rename(&mut self, save: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some((store, workspace_id)) = self.renaming_workspace.take() else {
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

    fn render_message(&self, store: &PaseoStore, cx: &App) -> Option<AnyElement> {
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
            ConnectionStatus::Connecting(_) | ConnectionStatus::Connected(_) => {
                let error = store.sync_error()?;
                (
                    IconName::Warning,
                    format!("Failed to load workspaces: {error}").into(),
                )
            }
        };
        Some(render_notice(icon, message, cx))
    }

    /// One daemon's projects. With several daemons they sit under a collapsible row with the
    /// daemon's status and Add Project.
    fn render_daemon(
        &self,
        store: &Entity<PaseoStore>,
        active_workspace: Option<&str>,
        with_header: bool,
        show_projects: bool,
        is_first: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let daemon = store.entity_id();
        let is_collapsed = with_header && show_projects && self.collapsed_daemons.contains(&daemon);
        let store_ref = store.read(cx);
        let is_connected = matches!(store_ref.status(), ConnectionStatus::Connected(_));
        let groups = store_ref.project_groups();
        let message = self.render_message(store_ref, cx);
        let colors = cx.theme().colors();
        let (panel_background, hover, border) = (
            colors.panel_background,
            colors.ghost_element_hover,
            colors.border,
        );
        let add_project = self.render_add_project_button(store, cx);
        let header = with_header.then(|| {
            h_flex()
                .id(SharedString::from(format!("paseo-daemon-{daemon}")))
                .h(Tab::container_height(cx))
                .w_full()
                .px_2()
                .gap_1()
                .bg(panel_background)
                .when(!is_first, |this| this.border_t_1())
                .border_b_1()
                .border_color(border)
                .when(show_projects, |this| {
                    this.cursor_pointer()
                        .hover(move |style| style.bg(hover))
                        .child(
                            Icon::new(if is_collapsed {
                                IconName::ChevronRight
                            } else {
                                IconName::ChevronDown
                            })
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                        )
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            if !this.collapsed_daemons.remove(&daemon) {
                                this.collapsed_daemons.insert(daemon);
                            }
                            cx.notify();
                        }))
                })
                .child(self.render_daemon_status(store, Color::Default, cx))
                .child(div().flex_1())
                .children(add_project)
        });
        let projects: Vec<AnyElement> = if is_collapsed || !show_projects {
            Vec::new()
        } else {
            groups
                .into_iter()
                .enumerate()
                .map(|(index, group)| {
                    self.render_project(store, group, active_workspace, index == 0, cx)
                })
                .collect()
        };
        v_flex()
            .w_full()
            .children(header)
            .when(!is_collapsed, |this| this.children(message))
            .child(
                v_flex()
                    .w_full()
                    .when(!is_connected, |this| this.opacity(0.6))
                    .children(projects),
            )
            .into_any_element()
    }

    fn render_status_groups(
        &self,
        stores: &[Entity<PaseoStore>],
        active_workspace: Option<&(Entity<PaseoStore>, String)>,
        several_daemons: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
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
        let mut sections = Vec::new();
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
            let (icon, color, label) = bucket_heading(bucket);
            let hover = cx.theme().colors().ghost_element_hover;
            let header = h_flex()
                .id(SharedString::from(format!("paseo-status-{bucket:?}")))
                .h(Tab::content_height(cx))
                .w_full()
                .px_2()
                .gap_1p5()
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .child(Icon::new(icon).size(IconSize::Small).color(color))
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                .when(is_collapsed, |this| {
                    this.child(
                        Label::new(bucket_rows.len().to_string())
                            .size(LabelSize::Small)
                            .color(Color::Disabled),
                    )
                })
                .on_click(cx.listener(move |this, _, _window, cx| {
                    if !this.collapsed_buckets.remove(&bucket) {
                        this.collapsed_buckets.insert(bucket);
                    }
                    cx.notify();
                }));
            let workspaces: Vec<AnyElement> = if is_collapsed {
                Vec::new()
            } else {
                bucket_rows
                    .into_iter()
                    .map(|(store, project, entry)| {
                        let is_active = active_workspace.is_some_and(|(active_store, id)| {
                            *active_store == store && *id == entry.workspace.id
                        });
                        let origin = RowOrigin {
                            branch: None,
                            project,
                            host: several_daemons.then(|| store.read(cx).display_name()),
                        };
                        self.render_workspace(&store, entry, is_active, Some(origin), cx)
                    })
                    .collect()
            };
            sections.push(
                v_flex()
                    .w_full()
                    .pt_1()
                    .child(header)
                    .children(workspaces)
                    .into_any_element(),
            );
        }
        sections
    }

    fn render_not_configured(&self, cx: &App) -> AnyElement {
        render_notice(
            IconName::Info,
            "Not connected to Paseo. Set the daemon's address in settings.json, for example \
             \"paseo\": { \"endpoint\": \"127.0.0.1:6767\" }."
                .into(),
            cx,
        )
    }

    fn render_project(
        &self,
        store: &Entity<PaseoStore>,
        group: ProjectGroup,
        active_workspace: Option<&str>,
        is_first: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let project_key = DaemonItemKey::new(store, &group.project_id);
        let is_collapsed = self.collapsed_projects.contains(&project_key);
        let attention_count = group
            .workspaces
            .iter()
            .filter(|entry| entry.workspace.status != WorkspaceStatus::Done)
            .count();
        let color = cx.theme().colors();

        let header = h_flex()
            .id(project_key.element_id("paseo-project"))
            .h(Tab::content_height(cx))
            .w_full()
            .px_2()
            .gap_1()
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
                Label::new(group.name.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            )
            .when(is_collapsed && attention_count > 0, |this| {
                this.child(Indicator::dot().color(Color::Accent))
            })
            .child(div().flex_1())
            .child(
                IconButton::new(
                    project_key.element_id("paseo-new-workspace"),
                    IconName::Plus,
                )
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text("New Workspace…"))
                .on_click(cx.listener({
                    let store = store.clone();
                    let project_id = group.project_id.clone();
                    let name = group.name.clone();
                    let root_path = group.root_path.clone();
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
                })),
            )
            .tooltip(Tooltip::text(group.root_path.clone()))
            .on_click(cx.listener(move |this, _, _window, cx| {
                if !this.collapsed_projects.remove(&project_key) {
                    this.collapsed_projects.insert(project_key.clone());
                }
                cx.notify();
            }));

        v_flex()
            .w_full()
            .when(!is_first, |this| {
                this.border_t_1().border_color(color.border_variant)
            })
            .child(header)
            .when(!is_collapsed, |this| {
                this.map(|this| {
                    if group.workspaces.is_empty() {
                        this.child(
                            div().pl_6().pb_1().child(
                                Label::new("No workspaces")
                                    .size(LabelSize::Small)
                                    .color(Color::Disabled),
                            ),
                        )
                    } else {
                        this.children(group.workspaces.into_iter().map(|entry| {
                            let is_active = active_workspace == Some(entry.workspace.id.as_str());
                            self.render_workspace(store, entry, is_active, None, cx)
                        }))
                    }
                })
            })
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
        let is_renaming = self
            .renaming_workspace
            .as_ref()
            .is_some_and(|(renaming_store, id)| renaming_store == store && *id == workspace.id);

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
        let worktree_name = workspace
            .worktree_slug
            .clone()
            .filter(|_| is_worktree && origin.is_none());
        let branch = branch.or_else(|| worktree_name.clone().filter(|_| origin.is_some()));
        let branch_name = branch.filter(|branch| Some(branch) != worktree_name.as_ref());
        // Rows with their project and daemon show the branch among those details instead, so
        // that it gives way before them.
        let mut origin = origin;
        let branch_name = match origin.as_mut() {
            Some(origin) => {
                origin.branch = branch_name.map(Into::into);
                None
            }
            None => branch_name,
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
            .hovered(is_hovered)
            .on_click(cx.listener({
                let store = store.clone();
                let workspace_id = workspace_id.clone();
                move |this, _, window, cx| {
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

fn connection_tooltip(store: &PaseoStore) -> SharedString {
    match store.status() {
        ConnectionStatus::Connected(server_info) => format!(
            "Connected to {} (Paseo {})",
            server_info.server_id,
            server_info.version.as_deref().unwrap_or("unknown version")
        )
        .into(),
        ConnectionStatus::Connecting(endpoint) => format!("Connecting to {endpoint}").into(),
        ConnectionStatus::Disconnected(reason) => {
            format!("{reason}. Reconnecting automatically.").into()
        }
        ConnectionStatus::NotConfigured => format!(
            "Set paseo.endpoint in settings.json (or {ENDPOINT_ENV_VAR}) to connect to a Paseo daemon"
        )
        .into(),
        ConnectionStatus::InvalidEndpoint(error) => error.clone(),
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

fn status_bucket(status: WorkspaceStatus) -> WorkspaceStatus {
    match status {
        WorkspaceStatus::Unknown => WorkspaceStatus::Done,
        status => status,
    }
}

fn bucket_heading(bucket: WorkspaceStatus) -> (IconName, Color, &'static str) {
    match bucket {
        WorkspaceStatus::NeedsInput => (IconName::Warning, Color::Warning, "Needs input"),
        WorkspaceStatus::Failed => (IconName::XCircle, Color::Error, "Failed"),
        WorkspaceStatus::Attention => (IconName::Check, Color::Success, "Ready to review"),
        WorkspaceStatus::Running => (IconName::LoadCircle, Color::Accent, "Working"),
        WorkspaceStatus::Done | WorkspaceStatus::Unknown => (IconName::Check, Color::Muted, "Done"),
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
    // The branch gives way first; the project and daemon are capped, the pull request stays.
    let detail = |icon: IconName, text: SharedString, capped: bool| {
        h_flex()
            .min_w_0()
            .gap_0p5()
            .map(|this| {
                if capped {
                    this.flex_shrink_0().max_w(rems(6.))
                } else {
                    this.min_w(rems(3.5))
                }
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
        parts.push(detail(IconName::Folder, origin.project, true));
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
                .any(|entry| {
                    matches!(
                        entry.workspace.status,
                        WorkspaceStatus::Attention
                            | WorkspaceStatus::NeedsInput
                            | WorkspaceStatus::Failed
                    )
                })
        })
    }

    fn side(&self, cx: &App) -> SidebarSide {
        AgentSettings::get_global(cx).sidebar_side()
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
        let header = self.render_header(window, cx);
        let stores = self.stores(cx);
        let several_daemons = stores.len() > 1;
        let sections: Vec<AnyElement> = if stores.is_empty() {
            vec![self.render_not_configured(cx)]
        } else if self.grouping == Grouping::Status {
            // The daemons' status rows (or only their problems, with one daemon), then every
            // daemon's workspaces by status.
            let mut sections: Vec<AnyElement> = stores
                .iter()
                .enumerate()
                .map(|(index, store)| {
                    self.render_daemon(store, None, several_daemons, false, index == 0, cx)
                })
                .collect();
            sections.extend(self.render_status_groups(
                &stores,
                active_workspace.as_ref(),
                several_daemons,
                cx,
            ));
            sections
        } else {
            stores
                .iter()
                .enumerate()
                .map(|(index, store)| {
                    let active = active_workspace
                        .as_ref()
                        .filter(|(active_store, _)| active_store == store)
                        .map(|(_, id)| id.as_str());
                    self.render_daemon(store, active, several_daemons, true, index == 0, cx)
                })
                .collect()
        };

        v_flex()
            .id("paseo-sidebar")
            .track_focus(&self.focus_handle)
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
                    .children(sections),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_groups_follow_paseos_order() {
        assert_eq!(
            STATUS_BUCKETS.map(|bucket| bucket_heading(bucket).2),
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
