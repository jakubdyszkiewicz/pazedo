use agent_settings::AgentSettings;
use anyhow::Result;
use gpui::{
    Action as _, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Pixels, Render, Subscription, WeakEntity, Window, actions,
};
use settings::Settings as _;
use std::{sync::Arc, time::Duration};
use ui::{Tooltip, prelude::*};
use workspace::{
    Pane, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    item::{Item, ItemHandle},
    pane,
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

/// How long opening an agent or terminal waits for a just-created workspace to add its panels,
/// which Zed does asynchronously, before falling back to the center pane.
const PANEL_WAIT: Duration = Duration::from_secs(2);

/// A dock panel with its own tab bar for Paseo agent sessions and terminals, so they sit apart
/// from the files in the center pane. Each Zed workspace has its own, so it shows the tabs of
/// the Paseo workspace that is open.
pub struct PaseoPanel {
    pane: Entity<Pane>,
    workspace: WeakEntity<Workspace>,
    position: DockPosition,
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
            pane.set_render_tab_bar_buttons(cx, |pane, _window, cx| {
                let zoomed = pane.is_zoomed();
                let zoom = IconButton::new("paseo-panel-zoom", IconName::Maximize)
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
                    });
                (None, Some(zoom.into_any_element()))
            });
            if let Some(callbacks) = cx.try_global::<workspace::PaneSearchBarCallbacks>() {
                let languages = Some(project.read(cx).languages().clone());
                let toolbar = pane.toolbar().clone();
                (callbacks.setup_search_bar)(languages, &toolbar, window, cx);
            }
            pane
        });

        let subscriptions = vec![
            cx.subscribe_in(&pane, window, Self::handle_pane_event),
            cx.observe(&pane, |_, _, cx| cx.notify()),
        ];
        Self {
            pane,
            workspace,
            position: DockPosition::Left,
            _subscriptions: subscriptions,
        }
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
            // Like the terminal panel: closing the last tab closes the dock and returns focus to
            // the center, instead of leaving an empty pane where the next close-tab keystroke
            // closes the window.
            pane::Event::RemovedItem { .. } if pane.read(cx).items_len() == 0 => {
                cx.emit(PanelEvent::Close);
            }
            pane::Event::ZoomIn => cx.emit(PanelEvent::ZoomIn),
            pane::Event::ZoomOut => cx.emit(PanelEvent::ZoomOut),
            _ => {}
        }
    }

    /// The open item of type `T` that matches, in the Paseo panel or (for tabs opened before
    /// the panel existed) the center pane.
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

    /// Waits until a just-opened workspace has added its Paseo panel (or gives up after
    /// [`PANEL_WAIT`]), so the first agent opened in it lands in the panel.
    pub(crate) async fn wait_for_panel(workspace: &Entity<Workspace>, cx: &mut AsyncWindowContext) {
        let start = std::time::Instant::now();
        while start.elapsed() < PANEL_WAIT {
            let has_panel =
                workspace.read_with(cx, |workspace, cx| workspace.panel::<Self>(cx).is_some());
            if has_panel {
                return;
            }
            cx.background_executor()
                .timer(Duration::from_millis(25))
                .await;
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn pane(&self) -> &Entity<Pane> {
        &self.pane
    }
}

fn is_paseo_item(item: &dyn ItemHandle) -> bool {
    item.downcast::<crate::PaseoAgentItem>().is_some()
        || item.downcast::<crate::PaseoTerminalItem>().is_some()
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
            .size_full()
            .relative()
            .child(self.pane.clone())
            .when(is_empty, |this| {
                this.child(
                    v_flex()
                        .absolute()
                        .inset_0()
                        .p_4()
                        .items_center()
                        .justify_center()
                        .child(
                            Label::new("Agents and terminals opened from the sidebar show here.")
                                .color(Color::Muted),
                        ),
                )
            })
    }
}
