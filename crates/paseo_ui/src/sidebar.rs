use agent_settings::{AgentSettings, THREADS_LIST_MAX_WIDTH, THREADS_LIST_MIN_WIDTH};
use chrono::{DateTime, Utc};
use collections::HashSet;
use gpui::{
    App, Context, Decorations, Entity, EventEmitter, FocusHandle, Focusable, Pixels, Render,
    ScrollHandle, SharedString, Stateful, Subscription, Task, WeakEntity, Window, px,
};
use paseo_client::protocol::{
    Agent, AgentStatus, AttentionReason, Workspace, WorkspaceKind, WorkspaceStatus,
};
use platform_title_bar::{SidebarWindowControls, TitleBarSettings, apply_title_bar_insets};
use settings::Settings as _;
use std::path::{Path, PathBuf};
use theme::{ActiveTheme, CLIENT_SIDE_DECORATION_ROUNDING};
use ui::{
    AgentThreadStatus, CommonAnimationExt as _, Indicator, Tab, ThreadItem, ThreadItemWorktreeInfo,
    Tooltip, WorktreeKind, prelude::*, utils::platform_title_bar_height,
};
use util::path_list::PathList;
use workspace::{
    CloseWindow, MultiWorkspace, MultiWorkspaceEvent, OpenMode, Sidebar, SidebarEvent, SidebarSide,
};

use crate::{
    agent_item::PaseoAgentItem,
    store::{ConnectionStatus, ENDPOINT_ENV_VAR, PaseoStore, ProjectGroup, WorkspaceEntry},
};

/// Lists the workspaces of a Paseo daemon, grouped by project, in place of Zed's threads
/// sidebar. Selecting a workspace opens its directory as the window's active Zed workspace.
pub struct PaseoSidebar {
    multi_workspace: WeakEntity<MultiWorkspace>,
    store: Entity<PaseoStore>,
    focus_handle: FocusHandle,
    width: Pixels,
    collapsed_projects: HashSet<String>,
    hovered_workspace: Option<String>,
    /// Several Paseo workspaces can share a directory; this remembers which one was clicked so
    /// only it is highlighted.
    selected_workspace: Option<String>,
    scroll_handle: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl PaseoSidebar {
    pub fn new(
        multi_workspace: Entity<MultiWorkspace>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = PaseoStore::global(cx);
        let subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe(&multi_workspace, |_, _, event: &MultiWorkspaceEvent, cx| {
                if let MultiWorkspaceEvent::ActiveWorkspaceChanged { .. } = event {
                    cx.notify();
                }
            }),
        ];
        Self {
            multi_workspace: multi_workspace.downgrade(),
            store,
            focus_handle: cx.focus_handle(),
            width: AgentSettings::get_global(cx).threads_sidebar.default_width,
            collapsed_projects: HashSet::default(),
            hovered_workspace: None,
            selected_workspace: None,
            scroll_handle: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    #[cfg(test)]
    pub(crate) fn store(&self) -> &Entity<PaseoStore> {
        &self.store
    }

    /// The workspace row to highlight: the one last clicked if it is still the open directory,
    /// otherwise the first row showing the open directory.
    pub(crate) fn highlighted_workspace(&self, cx: &App) -> Option<String> {
        active_workspace_id(
            &self.store.read(cx).project_groups(),
            self.active_directory(cx).as_deref(),
            self.selected_workspace.as_deref(),
        )
    }

    pub(crate) fn open_workspace(
        &mut self,
        workspace_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(task) = self.activate_workspace(workspace_id, window, cx) {
            task.detach_and_log_err(cx);
        }
    }

    /// Opens the agent's tab in its workspace. With no agent id, starts a new agent there.
    fn open_agent(
        &mut self,
        workspace_id: &str,
        agent_id: Option<String>,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(activate) = self.activate_workspace(workspace_id, window, cx) else {
            return;
        };
        cx.spawn_in(window, async move |_, cx| {
            let workspace = activate.await?;
            workspace.update_in(cx, |workspace, window, cx| {
                PaseoAgentItem::open(workspace, agent_id, title, window, cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn activate_workspace(
        &mut self,
        workspace_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<Entity<workspace::Workspace>>>> {
        let directory = PathBuf::from(self.store.read(cx).workspace(workspace_id)?.directory());
        self.selected_workspace = Some(workspace_id.to_string());
        cx.notify();
        let multi_workspace = self.multi_workspace.upgrade()?;
        Some(multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.find_or_create_local_workspace(
                PathList::new(&[directory]),
                None,
                None,
                OpenMode::Activate,
                None,
                window,
                cx,
            )
        }))
    }

    /// The single root directory of the window's active Zed workspace.
    fn active_directory(&self, cx: &App) -> Option<PathBuf> {
        let multi_workspace = self.multi_workspace.upgrade()?;
        let root_paths = multi_workspace.read(cx).workspace().read(cx).root_paths(cx);
        match root_paths.as_slice() {
            [path] => Some(path.to_path_buf()),
            _ => None,
        }
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

        let (indicator_color, status_text): (Color, SharedString) =
            match self.store.read(cx).status() {
                ConnectionStatus::Connected(server_info) => (
                    Color::Success,
                    server_info
                        .hostname
                        .clone()
                        .unwrap_or_else(|| server_info.server_id.clone())
                        .into(),
                ),
                ConnectionStatus::Connecting(_) => (Color::Warning, "Connecting…".into()),
                ConnectionStatus::Disconnected(_) => (Color::Error, "Disconnected".into()),
                ConnectionStatus::NotConfigured | ConnectionStatus::InvalidEndpoint(_) => {
                    (Color::Muted, "Not connected".into())
                }
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
            .child(
                h_flex()
                    .id("paseo-connection-status")
                    .min_w_0()
                    .gap_1()
                    .child(Indicator::dot().color(indicator_color))
                    .child(
                        Label::new(status_text)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .tooltip({
                        let tooltip = self.connection_tooltip(cx);
                        move |_window, cx| Tooltip::simple(tooltip.clone(), cx)
                    }),
            )
            .when(right_window_controls, |this| {
                this.child(div().flex_1()).children(
                    platform_title_bar::render_right_window_controls(
                        cx.button_layout(),
                        Box::new(CloseWindow),
                        window,
                    ),
                )
            })
            .into_any_element()
    }

    fn connection_tooltip(&self, cx: &App) -> SharedString {
        match self.store.read(cx).status() {
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
            ConnectionStatus::NotConfigured => {
                format!("Set {ENDPOINT_ENV_VAR} to connect to a Paseo daemon").into()
            }
            ConnectionStatus::InvalidEndpoint(error) => error.clone(),
        }
    }

    fn render_message(&self, cx: &App) -> Option<AnyElement> {
        let store = self.store.read(cx);
        let (icon, message): (IconName, SharedString) = match store.status() {
            ConnectionStatus::NotConfigured => (
                IconName::Info,
                format!(
                    "Not connected to Paseo. Set {ENDPOINT_ENV_VAR} to a daemon address \
                     (for example 127.0.0.1:6767) and restart."
                )
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
        Some(
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
                .into_any_element(),
        )
    }

    fn render_project(
        &self,
        group: ProjectGroup,
        active_workspace: Option<&str>,
        is_first: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_collapsed = self.collapsed_projects.contains(&group.project_id);
        let attention_count = group
            .workspaces
            .iter()
            .filter(|entry| entry.workspace.status != WorkspaceStatus::Done)
            .count();
        let project_id = group.project_id.clone();
        let color = cx.theme().colors();

        let header = h_flex()
            .id(SharedString::from(format!(
                "paseo-project-{}",
                group.project_id
            )))
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
            .tooltip(Tooltip::text(group.root_path.clone()))
            .on_click(cx.listener(move |this, _, _window, cx| {
                if !this.collapsed_projects.remove(&project_id) {
                    this.collapsed_projects.insert(project_id.clone());
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
                            self.render_workspace(entry, is_active, cx)
                        }))
                    }
                })
            })
            .into_any_element()
    }

    fn render_workspace(
        &self,
        entry: WorkspaceEntry,
        is_active: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace = &entry.workspace;
        let workspace_id = workspace.id.clone();
        let is_hovered = self.hovered_workspace.as_deref() == Some(workspace.id.as_str());

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
        // `name` is the branch unless the workspace was titled; show the branch only when it adds
        // information.
        let worktrees = match &branch {
            Some(branch) if is_worktree || branch != &workspace.name => {
                vec![ThreadItemWorktreeInfo {
                    worktree_name: workspace.worktree_slug.clone().map(Into::into),
                    branch_name: Some(branch.clone().into()),
                    full_path: workspace.directory().to_string().into(),
                    highlight_positions: Vec::new(),
                    kind: if is_worktree {
                        WorktreeKind::Linked
                    } else {
                        WorktreeKind::Main
                    },
                }]
            }
            _ => Vec::new(),
        };

        let mut item = ThreadItem::new(
            SharedString::from(format!("paseo-workspace-{}", workspace.id)),
            workspace.name.clone(),
        )
        .base_bg(cx.theme().colors().panel_background)
        .icon(if is_worktree {
            IconName::GitBranch
        } else {
            IconName::Folder
        })
        .status(status)
        .notified(notified)
        .worktrees(worktrees)
        .timestamp(
            workspace
                .activity_at
                .as_deref()
                .or(workspace.created_at.as_deref())
                .and_then(relative_time)
                .unwrap_or_default(),
        )
        .selected(is_active)
        .hovered(is_hovered)
        .on_hover(cx.listener({
            let workspace_id = workspace_id.clone();
            move |this, is_hovered: &bool, _window, cx| {
                if *is_hovered {
                    this.hovered_workspace = Some(workspace_id.clone());
                } else if this.hovered_workspace.as_ref() == Some(&workspace_id) {
                    this.hovered_workspace = None;
                }
                cx.notify();
            }
        }))
        .when(is_hovered, |item| {
            let workspace_id = workspace_id.clone();
            item.action_slot(
                IconButton::new(
                    SharedString::from(format!("paseo-new-agent-{}", workspace.id)),
                    IconName::Plus,
                )
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text("New Agent"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.open_agent(&workspace_id, None, "New Agent".into(), window, cx);
                })),
            )
        })
        .on_click(cx.listener(move |this, _, window, cx| {
            this.open_workspace(&workspace_id, window, cx);
        }));
        if let Some(diff_stat) = &workspace.diff_stat {
            if diff_stat.additions > 0 {
                item = item.added(diff_stat.additions as usize);
            }
            if diff_stat.deletions > 0 {
                item = item.removed(diff_stat.deletions as usize);
            }
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
        let open_workspace_id = workspace.id.clone();
        v_flex()
            .w_full()
            .child(
                div()
                    .id(SharedString::from(format!(
                        "paseo-workspace-row-{}",
                        workspace.id
                    )))
                    .w_full()
                    .child(item)
                    .tooltip(Tooltip::text(tooltip)),
            )
            .children(entry.agents.iter().map(|agent| {
                let workspace_id = open_workspace_id.clone();
                let agent_id = agent.id.clone();
                let title = agent_title(agent);
                self.render_agent(agent, cx)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_agent(
                            &workspace_id,
                            Some(agent_id.clone()),
                            title.clone(),
                            window,
                            cx,
                        );
                    }))
                    .into_any_element()
            }))
            .into_any_element()
    }
}

impl PaseoSidebar {
    /// One line per agent under its workspace: status, title and age. Clicking opens the
    /// agent's session in a tab.
    fn render_agent(&self, agent: &Agent, cx: &mut Context<Self>) -> Stateful<Div> {
        let color = cx.theme().colors();
        let waiting_for_permission = !agent.pending_permissions.is_empty()
            || agent.attention_reason == Some(AttentionReason::Permission);
        let failed = agent.status == AgentStatus::Error
            || agent.attention_reason == Some(AttentionReason::Error);
        let running = matches!(
            agent.status,
            AgentStatus::Running | AgentStatus::Initializing
        );

        let (status, status_text): (AnyElement, &str) = if waiting_for_permission {
            (
                Icon::new(IconName::Warning)
                    .size(IconSize::XSmall)
                    .color(Color::Warning)
                    .into_any_element(),
                "Waiting for permission",
            )
        } else if failed {
            (
                Icon::new(IconName::XCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Error)
                    .into_any_element(),
                "Failed",
            )
        } else if running {
            (
                Icon::new(IconName::LoadCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
                    .with_rotate_animation(2)
                    .into_any_element(),
                "Running",
            )
        } else if agent.requires_attention {
            (
                Indicator::dot().color(Color::Accent).into_any_element(),
                "Finished, not seen yet",
            )
        } else {
            (
                Icon::new(IconName::Check)
                    .size(IconSize::XSmall)
                    .color(Color::Disabled)
                    .into_any_element(),
                "Idle",
            )
        };

        let title = agent_title(agent);
        let highlighted = running || waiting_for_permission || failed || agent.requires_attention;
        let tooltip: SharedString = format!("{title}\n{} · {status_text}", agent.provider).into();

        h_flex()
            .id(SharedString::from(format!("paseo-agent-{}", agent.id)))
            .w_full()
            .h_6()
            .pl(px(30.))
            .pr_2()
            .gap_1p5()
            .cursor_pointer()
            .hover(|style| style.bg(color.ghost_element_hover))
            .child(
                div()
                    .flex_none()
                    .size_3()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(status),
            )
            .child(
                Label::new(title)
                    .size(LabelSize::Small)
                    .color(if highlighted {
                        Color::Default
                    } else {
                        Color::Muted
                    })
                    .truncate(),
            )
            .child(div().flex_1())
            .children(
                relative_time(&agent.updated_at)
                    .map(|age| Label::new(age).size(LabelSize::XSmall).color(Color::Muted)),
            )
            .tooltip(Tooltip::text(tooltip))
    }
}

fn agent_title(agent: &Agent) -> SharedString {
    agent
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| format!("{} agent", agent.provider))
        .into()
}

fn active_workspace_id(
    groups: &[ProjectGroup],
    active_directory: Option<&Path>,
    selected: Option<&str>,
) -> Option<String> {
    let active_directory = active_directory?;
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

fn relative_time(timestamp: &str) -> Option<String> {
    let time: DateTime<Utc> = DateTime::parse_from_rfc3339(timestamp).ok()?.into();
    let elapsed = Utc::now().signed_duration_since(time);
    Some(if elapsed.num_minutes() < 1 {
        "now".to_string()
    } else if elapsed.num_hours() < 1 {
        format!("{}m", elapsed.num_minutes())
    } else if elapsed.num_days() < 1 {
        format!("{}h", elapsed.num_hours())
    } else if elapsed.num_weeks() < 1 {
        format!("{}d", elapsed.num_days())
    } else {
        format!("{}w", elapsed.num_weeks())
    })
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
        self.store
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
        let groups = self.store.read(cx).project_groups();
        let is_connected = matches!(self.store.read(cx).status(), ConnectionStatus::Connected(_));
        let header = self.render_header(window, cx);
        let message = self.render_message(cx);
        let projects: Vec<AnyElement> = groups
            .into_iter()
            .enumerate()
            .map(|(index, group)| {
                self.render_project(group, active_workspace.as_deref(), index == 0, cx)
            })
            .collect();

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
            .children(message)
            .child(
                v_flex()
                    .id("paseo-sidebar-projects")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .when(!is_connected, |this| this.opacity(0.6))
                    .children(projects),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_relative_times() {
        let now = Utc::now();
        let ago = |duration: chrono::Duration| (now - duration).to_rfc3339();
        assert_eq!(
            relative_time(&ago(chrono::Duration::seconds(5))).as_deref(),
            Some("now")
        );
        assert_eq!(
            relative_time(&ago(chrono::Duration::minutes(7))).as_deref(),
            Some("7m")
        );
        assert_eq!(
            relative_time(&ago(chrono::Duration::hours(3))).as_deref(),
            Some("3h")
        );
        assert_eq!(
            relative_time(&ago(chrono::Duration::days(2))).as_deref(),
            Some("2d")
        );
        assert_eq!(relative_time("not a time"), None);
    }
}
