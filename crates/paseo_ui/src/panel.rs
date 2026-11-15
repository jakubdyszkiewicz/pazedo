use agent_settings::AgentSettings;
use anyhow::Result;
use collections::HashSet;
use gpui::{
    Action as _, App, AsyncWindowContext, Context, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, Pixels, Render, SharedString, Subscription, WeakEntity, Window, actions,
};
use paseo_client::protocol::{Agent, ProviderEntry, ProviderStatus};
use settings::Settings as _;
use std::{path::PathBuf, rc::Rc, sync::Arc, time::Duration};
use ui::{ContextMenu, PopoverMenu, PopoverMenuHandle, Tooltip, prelude::*};
use util::path_list::PathList;
use workspace::{
    Pane, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    item::{Item, ItemHandle},
    pane,
};

use crate::{
    PaseoAgentItem, PaseoDaemons, PaseoStore, PaseoTerminalItem,
    agent_session::{NewAgent, PROVIDER_ENV_VAR},
};

actions!(
    paseo_panel,
    [
        /// Toggles focus on the Paseo panel, which holds Paseo agent and terminal tabs.
        ToggleFocus
    ]
);

/// pazedo shows Paseo agents in [`PaseoPanel`], so Zed's own agent panel is not registered
/// (see `ensure_agent_panel_for_workspace` in `crates/zed/src/zed.rs`).
pub const REPLACES_ZED_AGENT_PANEL: bool = true;

/// How long opening a workspace waits for a just-created Zed workspace to add its panels,
/// which Zed does asynchronously.
const PANEL_WAIT: Duration = Duration::from_secs(2);

/// A dock panel whose tabs are the agents and terminals of the Paseo workspace open in this Zed
/// workspace, kept in step with the daemon: agents and terminals started anywhere (the phone,
/// the CLI, other agents) get a tab, and closing a tab archives the agent or kills the terminal.
pub struct PaseoPanel {
    pane: Entity<Pane>,
    workspace: WeakEntity<Workspace>,
    daemons: Entity<PaseoDaemons>,
    /// The daemon of the Paseo workspace whose tabs are shown.
    store: Option<Entity<PaseoStore>>,
    /// The Paseo workspace whose tabs are shown, when the Zed workspace's folder is one.
    paseo_workspace_id: Option<String>,
    /// Agents and terminals that had a tab here. One that disappears from the store was closed
    /// or archived elsewhere, unlike a just-created agent the store does not know yet.
    known_items: HashSet<String>,
    /// Set while the panel adds and removes tabs itself, which must not close the dock.
    syncing: bool,
    position: DockPosition,
    _store_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl PaseoPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
                workspace.toggle_panel_focus::<PaseoPanel>(window, cx);
            });
            let weak_workspace = workspace.weak_handle();
            let project = workspace.project().clone();
            cx.new(|cx| Self::new(weak_workspace, project, window, cx))
        })
    }

    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<project::Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel = cx.entity().downgrade();
        let new_menu = PopoverMenuHandle::default();
        let pane = cx.new(|cx| {
            let mut pane = Pane::new(
                workspace.clone(),
                project.clone(),
                Default::default(),
                // Only Paseo tabs can be dropped here; file tabs belong in the center.
                Some(Arc::new(|dragged, _window, _cx| {
                    dragged
                        .downcast_ref::<workspace::DraggedTab>()
                        .is_some_and(|tab| is_paseo_item(tab.item.as_ref()))
                })),
                ToggleFocus.boxed_clone(),
                false,
                window,
                cx,
            );
            pane.set_can_navigate(false, cx);
            pane.display_nav_history_buttons(None);
            pane.set_should_display_tab_bar(|_, _| true);
            pane.set_close_pane_if_empty(false, cx);
            pane.set_zoom_out_on_close(false);
            pane.set_can_split(Some(Arc::new(|_, _, _, _| false)));
            pane.set_render_tab_bar_buttons(cx, {
                let new_menu = new_menu.clone();
                move |pane, _window, cx| {
                    let zoomed = pane.is_zoomed();
                    let buttons = h_flex()
                        .gap(DynamicSpacing::Base02.rems(cx))
                        .child(new_item_menu(
                            "paseo-panel-new",
                            panel.clone(),
                            new_menu.clone(),
                            true,
                            IconButton::new("paseo-panel-new-button", IconName::Plus)
                                .icon_size(IconSize::Small),
                            "New Agent or Terminal",
                        ))
                        .child(
                            IconButton::new("paseo-panel-zoom", IconName::Maximize)
                                .icon_size(IconSize::Small)
                                .toggle_state(zoomed)
                                .selected_icon(IconName::Minimize)
                                .on_click(cx.listener(|pane, _, window, cx| {
                                    pane.toggle_zoom(&workspace::ToggleZoom, window, cx);
                                }))
                                .tooltip(move |_window, cx| {
                                    Tooltip::for_action(
                                        if zoomed { "Zoom Out" } else { "Zoom In" },
                                        &workspace::ToggleZoom,
                                        cx,
                                    )
                                }),
                        );
                    (None, Some(buttons.into_any_element()))
                }
            });
            if let Some(callbacks) = cx.try_global::<workspace::PaneSearchBarCallbacks>() {
                let languages = Some(project.read(cx).languages().clone());
                let toolbar = pane.toolbar().clone();
                (callbacks.setup_search_bar)(languages, &toolbar, window, cx);
            }
            pane
        });

        let daemons = PaseoDaemons::global(cx);
        let subscriptions = vec![
            cx.subscribe_in(&pane, window, Self::handle_pane_event),
            cx.observe(&pane, |_, _, cx| cx.notify()),
            cx.observe_in(&daemons, window, |this, _, window, cx| {
                this.observe_stores(window, cx);
                this.sync_tabs(window, cx);
            }),
        ];
        cx.defer_in(window, |this, window, cx| this.sync_tabs(window, cx));
        let mut this = Self {
            pane,
            workspace,
            daemons,
            store: None,
            paseo_workspace_id: None,
            known_items: HashSet::default(),
            syncing: false,
            position: DockPosition::Left,
            _store_subscriptions: Vec::new(),
            _subscriptions: subscriptions,
        };
        this.observe_stores(window, cx);
        this
    }

    fn observe_stores(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._store_subscriptions = self
            .daemons
            .read(cx)
            .stores()
            .to_vec()
            .iter()
            .map(|store| {
                cx.observe_in(store, window, |this, _, window, cx| {
                    this.sync_tabs(window, cx)
                })
            })
            .collect();
    }

    fn handle_pane_event(
        &mut self,
        pane: &Entity<Pane>,
        event: &pane::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            // Subscribes the workspace to the item's events, e.g. a terminal tab closing itself
            // when its shell exits.
            pane::Event::AddItem { item } => {
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        item.added_to_pane(workspace, pane.clone(), window, cx)
                    });
                }
            }
            // After the last tab closes the panel shows its New Agent / New Terminal screen.
            // Focus moves to the center, or the next close-tab keystroke in the empty pane would
            // close the window.
            pane::Event::RemovedItem { .. } if !self.syncing && pane.read(cx).items_len() == 0 => {
                if let Some(workspace) = self.workspace.upgrade() {
                    let center = workspace.read(cx).active_pane().focus_handle(cx);
                    window.focus(&center, cx);
                }
            }
            pane::Event::ZoomIn => cx.emit(PanelEvent::ZoomIn),
            pane::Event::ZoomOut => cx.emit(PanelEvent::ZoomOut),
            _ => {}
        }
    }

    /// The single folder of this Zed workspace, which names its Paseo workspace.
    fn directory(&self, cx: &App) -> Option<PathBuf> {
        let workspace = self.workspace.upgrade()?;
        match workspace.read(cx).root_paths(cx).as_slice() {
            [path] => Some(path.to_path_buf()),
            _ => None,
        }
    }

    /// Makes the tabs match the Paseo workspace's agents and terminals: adds a tab for each one
    /// without a tab, and removes tabs of agents and terminals that were archived, killed, or
    /// belong to another Paseo workspace of the same folder.
    fn sync_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(directory) = self.directory(cx) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let found = self.daemons.read(cx).workspace_for_folder(
            workspace.read(cx).project().read(cx),
            &directory,
            cx,
        );
        let Some((store, id)) = found else {
            self.store = None;
            if self.paseo_workspace_id.take().is_some() {
                cx.notify();
            }
            return;
        };
        self.store = Some(store.clone());
        let (paseo_workspace_id, watching, agents, terminals) = {
            let store = store.read(cx);
            let agents: Vec<(String, SharedString)> = store
                .workspace_agents(&id)
                .into_iter()
                .map(|agent| (agent.id.clone(), agent_title(agent)))
                .collect();
            let terminals: Vec<(String, SharedString)> = store
                .workspace_terminals(&id)
                .into_iter()
                .map(|terminal| (terminal.id.clone(), terminal.name.clone().into()))
                .collect();
            (
                id.clone(),
                store.is_watching_terminals(&id),
                agents,
                terminals,
            )
        };
        if self.paseo_workspace_id.as_ref() != Some(&paseo_workspace_id) {
            self.paseo_workspace_id = Some(paseo_workspace_id.clone());
            cx.notify();
        }
        if !watching {
            let store = store.clone();
            cx.defer(move |cx| {
                store.update(cx, |store, cx| {
                    store.watch_terminals(&paseo_workspace_id, cx)
                });
            });
        }

        let store_entity = store;
        let store = store_entity.read(cx);
        let mut stale = Vec::new();
        for item in self.pane.read(cx).items() {
            if let Some(agent_item) = item.downcast::<PaseoAgentItem>()
                && let Some(agent_id) = agent_item.read(cx).agent_id(cx)
                && !agents.iter().any(|(id, _)| *id == agent_id)
                && (store.agent(&agent_id).is_some() || self.known_items.contains(&agent_id))
            {
                agent_item.read(cx).detach();
                stale.push(item.item_id());
            } else if let Some(terminal_item) = item.downcast::<PaseoTerminalItem>() {
                let terminal_id = terminal_item.read(cx).terminal_id();
                if !terminals.iter().any(|(id, _)| id == terminal_id)
                    && (store.terminal(terminal_id).is_some()
                        || self.known_items.contains(terminal_id))
                {
                    terminal_item.read(cx).detach();
                    stale.push(item.item_id());
                }
            }
        }

        // Not `PaseoAgentItem::find`: that looks the panel up through the workspace, and this
        // panel is being updated.
        let workspace_ref = workspace.read(cx);
        let pane = self.pane.read(cx);
        // A tab that starts an agent learns the agent's id only after the daemon announced the
        // agent; until then a new agent may be that tab's, so it gets no tab of its own yet.
        let starting_agent = pane
            .items_of_type::<PaseoAgentItem>()
            .chain(workspace_ref.items_of_type::<PaseoAgentItem>(cx))
            .any(|item| item.read(cx).agent_id(cx).is_none());
        let missing_agents: Vec<_> = agents
            .iter()
            .filter(|(id, _)| !(starting_agent && !self.known_items.contains(id)))
            .filter(|(id, _)| {
                let matches = |item: &Entity<PaseoAgentItem>| {
                    item.read(cx).agent_id(cx).as_deref() == Some(id.as_str())
                };
                !pane
                    .items_of_type::<PaseoAgentItem>()
                    .any(|item| matches(&item))
                    && !workspace_ref
                        .items_of_type::<PaseoAgentItem>(cx)
                        .any(|item| matches(&item))
            })
            .cloned()
            .collect();
        let missing_terminals: Vec<_> = terminals
            .iter()
            .filter(|(id, _)| {
                let matches =
                    |item: &Entity<PaseoTerminalItem>| item.read(cx).terminal_id() == id.as_str();
                !pane
                    .items_of_type::<PaseoTerminalItem>()
                    .any(|item| matches(&item))
                    && !workspace_ref
                        .items_of_type::<PaseoTerminalItem>(cx)
                        .any(|item| matches(&item))
            })
            .cloned()
            .collect();
        if !starting_agent {
            self.known_items
                .extend(agents.into_iter().map(|(id, _)| id));
        }
        self.known_items
            .extend(terminals.into_iter().map(|(id, _)| id));
        if stale.is_empty() && missing_agents.is_empty() && missing_terminals.is_empty() {
            return;
        }

        let weak_workspace = workspace.downgrade();
        let workspace_ref = workspace.read(cx);
        let project = workspace_ref.project().clone();
        let work_dirs = PathList::new(&workspace_ref.root_paths(cx));
        let database_id = workspace_ref.database_id();
        let mut new_items: Vec<Box<dyn ItemHandle>> = Vec::new();
        for (agent_id, title) in missing_agents {
            new_items.push(Box::new(cx.new(|cx| {
                PaseoAgentItem::new(
                    store_entity.clone(),
                    Some(agent_id),
                    None,
                    title,
                    weak_workspace.clone(),
                    project.clone(),
                    work_dirs.clone(),
                    cx,
                )
            })));
        }
        for (terminal_id, name) in missing_terminals {
            let item = cx.new(|cx| {
                PaseoTerminalItem::new(
                    store_entity.clone(),
                    terminal_id,
                    name,
                    weak_workspace.clone(),
                    database_id,
                    project.downgrade(),
                    window,
                    cx,
                )
            });
            new_items.push(Box::new(item));
        }

        self.syncing = true;
        self.pane.update(cx, |pane, cx| {
            for item_id in stale {
                pane.remove_item(item_id, false, false, window, cx);
            }
            // Adding a tab activates it; background additions keep the tab the user is on.
            let active = pane.active_item().map(|item| item.item_id());
            for item in new_items {
                pane.add_item(item, false, false, None, window, cx);
            }
            if let Some(index) = active.and_then(|id| item_index(pane, id)) {
                pane.activate_item(index, false, false, window, cx);
            }
        });
        self.syncing = false;
    }

    /// Opens a tab that starts a new agent. Takes the panel's workspace rather than the panel:
    /// opening the tab looks the panel up, which must not happen while it is being updated.
    fn new_agent(
        workspace: &WeakEntity<Workspace>,
        store: Entity<PaseoStore>,
        new_agent: NewAgent,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(workspace) = workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            PaseoAgentItem::open(
                workspace,
                store,
                None,
                Some(new_agent),
                "New Agent".into(),
                window,
                cx,
            );
        });
    }

    fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(store), Some(paseo_workspace_id)) =
            (self.store.clone(), self.paseo_workspace_id.clone())
        else {
            return;
        };
        let create = store.update(cx, |store, cx| {
            store.create_terminal(&paseo_workspace_id, cx)
        });
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            let terminal = create.await?;
            workspace.update_in(cx, |workspace, window, cx| {
                PaseoTerminalItem::open(
                    workspace,
                    store,
                    terminal.id,
                    terminal.name.into(),
                    window,
                    cx,
                );
            })
        })
        .detach_and_log_err(cx);
    }

    /// The open item of type `T` that matches, in the Paseo panel or (for tabs dragged there)
    /// the center pane.
    pub(crate) fn find_item<T: Item>(
        workspace: &Workspace,
        cx: &App,
        matches: impl Fn(&T, &App) -> bool,
    ) -> Option<Entity<T>> {
        let in_panel = workspace.panel::<Self>(cx).and_then(|panel| {
            panel
                .read(cx)
                .pane
                .read(cx)
                .items_of_type::<T>()
                .find(|item| matches(item.read(cx), cx))
        });
        in_panel.or_else(|| {
            workspace
                .items_of_type::<T>(cx)
                .find(|item| matches(item.read(cx), cx))
        })
    }

    /// Whether the item is still in the Paseo panel or a center pane, e.g. after a drag.
    pub(crate) fn is_open(workspace: &Workspace, item_id: EntityId, cx: &App) -> bool {
        let in_panel = workspace
            .panel::<Self>(cx)
            .is_some_and(|panel| item_index(panel.read(cx).pane.read(cx), item_id).is_some());
        in_panel
            || workspace
                .panes()
                .iter()
                .any(|pane| item_index(pane.read(cx), item_id).is_some())
    }

    /// Shows `item` in the Paseo panel, opening the panel, or in the center pane when the
    /// workspace has no Paseo panel. Activates the item if it is already open.
    pub(crate) fn show_item(
        workspace: &mut Workspace,
        item: Box<dyn ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(panel) = workspace.panel::<Self>(cx) else {
            match workspace.pane_for(item.as_ref()) {
                Some(pane) => pane.update(cx, |pane, cx| {
                    if let Some(index) = pane.index_for_item(item.as_ref()) {
                        pane.activate_item(index, true, true, window, cx);
                    }
                }),
                None => workspace.add_item_to_active_pane(item, None, true, window, cx),
            }
            return;
        };
        workspace.reveal_panel::<Self>(window, cx);
        let pane = panel.read(cx).pane.clone();
        pane.update(cx, |pane, cx| match pane.index_for_item(item.as_ref()) {
            Some(index) => pane.activate_item(index, true, true, window, cx),
            None => pane.add_item(item, true, true, None, window, cx),
        });
    }

    /// Opens the Paseo panel of a workspace just opened from the sidebar, on the tab of the
    /// agent that most recently finished unseen, if any, so opening an unread workspace shows
    /// what is new. A new Zed workspace adds its panels asynchronously, so this waits for the
    /// panel (or gives up after [`PANEL_WAIT`]).
    pub(crate) async fn reveal_when_ready(
        workspace: &Entity<Workspace>,
        cx: &mut AsyncWindowContext,
    ) -> Result<()> {
        let start = std::time::Instant::now();
        while start.elapsed() < PANEL_WAIT {
            let has_panel =
                workspace.read_with(cx, |workspace, cx| workspace.panel::<Self>(cx).is_some());
            if has_panel {
                return workspace.update_in(cx, |workspace, window, cx| {
                    workspace.reveal_panel::<Self>(window, cx);
                    if let Some(item) = Self::newest_unseen_agent(workspace, cx) {
                        Self::show_item(workspace, Box::new(item), window, cx);
                    }
                });
            }
            cx.background_executor()
                .timer(Duration::from_millis(25))
                .await;
        }
        Ok(())
    }

    fn newest_unseen_agent(workspace: &Workspace, cx: &App) -> Option<Entity<PaseoAgentItem>> {
        let panel = workspace.panel::<Self>(cx)?;
        let panel = panel.read(cx);
        let paseo_workspace_id = panel.paseo_workspace_id.as_deref()?;
        let store = panel.store.as_ref()?.read(cx);
        let agent = store
            .workspace_agents(paseo_workspace_id)
            .into_iter()
            .filter(|agent| store.has_unseen_attention(&agent.id))
            .max_by(|a, b| a.attention_timestamp.cmp(&b.attention_timestamp))?;
        PaseoAgentItem::find(workspace, &agent.id, cx)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn pane(&self) -> &Entity<Pane> {
        &self.pane
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = cx.entity().downgrade();
        let content = match &self.paseo_workspace_id {
            None => v_flex().child(
                Label::new(
                    "Open a Paseo workspace from the sidebar to see its agents and terminals.",
                )
                .color(Color::Muted),
            ),
            Some(_) => v_flex()
                .gap_2()
                .w(px(220.))
                .child(new_item_menu(
                    "paseo-panel-empty-new-agent",
                    panel,
                    PopoverMenuHandle::default(),
                    false,
                    Button::new("paseo-panel-empty-new-agent-button", "New Agent")
                        .full_width()
                        .style(ButtonStyle::Outlined)
                        .start_icon(Icon::new(IconName::Sparkle).size(IconSize::Small)),
                    "Choose a provider and model",
                ))
                .child(
                    Button::new("paseo-panel-empty-new-terminal", "New Terminal")
                        .full_width()
                        .style(ButtonStyle::Outlined)
                        .start_icon(Icon::new(IconName::Terminal).size(IconSize::Small))
                        .on_click(cx.listener(|this, _, window, cx| this.new_terminal(window, cx))),
                ),
        };
        v_flex()
            .absolute()
            .inset_0()
            .p_4()
            .items_center()
            .justify_center()
            .bg(cx.theme().colors().panel_background)
            .child(content)
    }
}

/// A menu of "New Terminal" (when `with_terminal`) and one entry per provider model, opened by
/// `trigger`.
fn new_item_menu(
    id: &'static str,
    panel: WeakEntity<PaseoPanel>,
    handle: PopoverMenuHandle<ContextMenu>,
    with_terminal: bool,
    trigger: impl ui::PopoverTrigger + ui::ButtonCommon,
    tooltip: &'static str,
) -> impl IntoElement {
    PopoverMenu::new(id)
        .with_handle(handle)
        .anchor(gpui::Anchor::TopRight)
        .trigger_with_tooltip(trigger, Tooltip::text(tooltip))
        .on_open(Rc::new({
            let panel = panel.clone();
            move |_, cx| {
                if let Ok(Some(store)) = panel.read_with(cx, |panel, _| panel.store.clone()) {
                    store.update(cx, |store, cx| store.refresh_providers(cx));
                }
            }
        }))
        .menu(move |window, cx| {
            let store = panel.read_with(cx, |panel, _| panel.store.clone()).ok()??;
            let choices = new_agent_choices(store.read(cx).providers());
            let panel = panel.clone();
            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                if with_terminal {
                    let panel = panel.clone();
                    menu = menu
                        .entry("New Terminal", None, move |window, cx| {
                            panel
                                .update(cx, |panel, cx| panel.new_terminal(window, cx))
                                .ok();
                        })
                        .separator();
                }
                if choices.is_empty() {
                    return menu.header("No agent providers available yet");
                }
                let mut current_header = None;
                for choice in choices {
                    if current_header.as_ref() != Some(&choice.header) {
                        menu = menu.header(choice.header.clone());
                        current_header = Some(choice.header.clone());
                    }
                    let panel = panel.clone();
                    let store = store.clone();
                    let new_agent = choice.new_agent;
                    menu = menu.entry(choice.label, None, move |window, cx| {
                        if let Ok(workspace) =
                            panel.read_with(cx, |panel, _| panel.workspace.clone())
                        {
                            PaseoPanel::new_agent(
                                &workspace,
                                store.clone(),
                                new_agent.clone(),
                                window,
                                cx,
                            );
                        }
                    });
                }
                menu
            }))
        })
}

fn item_index(pane: &Pane, item_id: EntityId) -> Option<usize> {
    pane.items().position(|item| item.item_id() == item_id)
}

fn is_paseo_item(item: &dyn ItemHandle) -> bool {
    item.downcast::<PaseoAgentItem>().is_some() || item.downcast::<PaseoTerminalItem>().is_some()
}

pub(crate) fn agent_title(agent: &Agent) -> SharedString {
    agent
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| format!("{} agent", agent.provider))
        .into()
}

pub(crate) struct NewAgentChoice {
    pub(crate) header: SharedString,
    pub(crate) label: SharedString,
    pub(crate) new_agent: NewAgent,
}

/// One entry per selectable model, grouped by provider, default model first. The provider set
/// with `PAZEDO_AGENT_PROVIDER` comes first, and is offered even if the daemon does not list it,
/// since setting it is a deliberate override.
/// One entry per usable provider. The agent starts on the provider's default model (or
/// `PAZEDO_AGENT_MODEL` for `PAZEDO_AGENT_PROVIDER`); the thread's model picker changes it, so
/// the menu stays short however many models the providers offer.
pub(crate) fn new_agent_choices(providers: &[ProviderEntry]) -> Vec<NewAgentChoice> {
    let header: SharedString = "New Agent".into();
    let mut choices = Vec::new();
    let from_env = NewAgent::from_env();
    let provider_from_env = std::env::var(PROVIDER_ENV_VAR).is_ok();
    if provider_from_env
        && !providers
            .iter()
            .any(|provider| provider.provider == from_env.provider)
    {
        choices.push(NewAgentChoice {
            header: header.clone(),
            label: format!("{} (from {PROVIDER_ENV_VAR})", from_env.provider).into(),
            new_agent: from_env.clone(),
        });
    }
    // The provider chosen with the environment variable is listed first.
    let mut providers: Vec<&ProviderEntry> = providers.iter().collect();
    if provider_from_env {
        providers.sort_by_key(|provider| provider.provider != from_env.provider);
    }
    for provider in providers {
        if !provider.enabled
            || matches!(
                provider.status,
                ProviderStatus::Unavailable | ProviderStatus::Error
            )
        {
            continue;
        }
        let label: SharedString = if provider.status == ProviderStatus::Loading {
            format!("{} (loading)", provider.label()).into()
        } else {
            provider.label().to_string().into()
        };
        let model = if provider_from_env && provider.provider == from_env.provider {
            from_env.model.clone()
        } else {
            None
        };
        choices.push(NewAgentChoice {
            header: header.clone(),
            label,
            new_agent: NewAgent {
                provider: provider.provider.clone(),
                model,
            },
        });
    }
    choices
}

impl EventEmitter<PanelEvent> for PaseoPanel {}

impl Focusable for PaseoPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.pane.focus_handle(cx)
    }
}

impl Panel for PaseoPanel {
    fn persistent_name() -> &'static str {
        "PaseoPanel"
    }

    fn panel_key() -> &'static str {
        "paseo_panel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, _position: DockPosition) -> bool {
        true
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, cx: &App) -> Pixels {
        AgentSettings::get_global(cx).default_width
    }

    fn supports_flexible_size(&self) -> bool {
        true
    }

    fn is_zoomed(&self, _window: &Window, cx: &App) -> bool {
        self.pane.read(cx).is_zoomed()
    }

    fn set_zoomed(&mut self, zoomed: bool, _window: &mut Window, cx: &mut Context<Self>) {
        self.pane.update(cx, |pane, cx| pane.set_zoomed(zoomed, cx));
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Sparkle)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Paseo Panel")
    }

    fn icon_label(&self, _window: &Window, cx: &App) -> Option<String> {
        match self.pane.read(cx).items_len() {
            0 => None,
            count => Some(count.to_string()),
        }
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn pane(&self) -> Option<Entity<Pane>> {
        Some(self.pane.clone())
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

impl Render for PaseoPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let container = cx
            .try_global::<workspace::PaneSearchBarCallbacks>()
            .map(|callbacks| (callbacks.wrap_div_with_search_actions)(div(), self.pane.clone()))
            .unwrap_or_else(div);
        let is_empty = self.pane.read(cx).items_len() == 0;
        container
            .key_context("PaseoPanel")
            .size_full()
            .relative()
            .child(self.pane.clone())
            .when(is_empty, |this| this.child(self.render_empty_state(cx)))
    }
}
