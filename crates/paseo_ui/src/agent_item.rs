use agent_client_protocol::v2::schema as acp_v2;
use agent_ui::{Agent, AgentConnectionStore, AgentThreadSource, ConversationView};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Subscription, WeakEntity, Window,
};
use project::{AgentId, Project};
use std::{cell::Cell, rc::Rc};
use ui::prelude::*;
use util::path_list::PathList;
use workspace::{Item, Workspace};

use crate::{
    agent_session::{NewAgent, PASEO_AGENT_ID, PaseoAgentServer, PaseoConnection},
    panel::PaseoPanel,
};

/// A Paseo agent session in a tab of the Paseo panel, rendered by Zed's thread UI.
pub struct PaseoAgentItem {
    /// `None` for a tab that started a new agent; its id is then the thread's session id.
    agent_id: Option<String>,
    /// For a tab that starts a new agent: the provider and model to start it with.
    new_agent: Option<NewAgent>,
    title: SharedString,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    work_dirs: PathList,
    /// Built when the tab is first shown, so a workspace with many agents does not load every
    /// agent's history at once.
    conversation_view: Option<Entity<ConversationView>>,
    focus_handle: FocusHandle,
    /// Set when pazedo removes the tab itself. Only a tab the user closes archives its agent.
    detached: Cell<bool>,
    _conversation_subscription: Option<Subscription>,
}

impl PaseoAgentItem {
    /// Opens the agent's tab in the workspace's Paseo panel, or focuses it if it is already
    /// open. With no agent id it starts a new agent with `new_agent`'s provider and model.
    pub fn open(
        workspace: &mut Workspace,
        agent_id: Option<String>,
        new_agent: Option<NewAgent>,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if let Some(agent_id) = &agent_id
            && let Some(existing) = Self::find(workspace, agent_id, cx)
        {
            PaseoPanel::show_item(workspace, Box::new(existing), window, cx);
            return;
        }
        let new_agent = agent_id
            .is_none()
            .then(|| new_agent.unwrap_or_else(NewAgent::from_env));
        let weak_workspace = workspace.weak_handle();
        let project = workspace.project().clone();
        let work_dirs = PathList::new(&workspace.root_paths(cx));
        let item = cx.new(|cx| {
            Self::new(
                agent_id,
                new_agent,
                title,
                weak_workspace,
                project,
                work_dirs,
                cx,
            )
        });
        PaseoPanel::show_item(workspace, Box::new(item), window, cx);
    }

    pub(crate) fn new(
        agent_id: Option<String>,
        new_agent: Option<NewAgent>,
        title: SharedString,
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            agent_id,
            new_agent,
            title,
            workspace,
            project,
            work_dirs,
            conversation_view: None,
            focus_handle: cx.focus_handle(),
            detached: Cell::new(false),
            _conversation_subscription: None,
        }
    }

    pub(crate) fn find(workspace: &Workspace, agent_id: &str, cx: &App) -> Option<Entity<Self>> {
        PaseoPanel::find_item::<Self>(workspace, cx, |item, cx| {
            item.agent_id(cx).as_deref() == Some(agent_id)
        })
    }

    pub(crate) fn agent_id(&self, cx: &App) -> Option<String> {
        self.agent_id.clone().or_else(|| {
            self.conversation_view
                .as_ref()?
                .read(cx)
                .active_thread()
                .map(|thread| thread.read(cx).thread.read(cx).session_id().0.to_string())
        })
    }

    /// Marks the tab as removed by pazedo, e.g. because the agent was archived elsewhere, so
    /// removing it does not archive the agent.
    pub(crate) fn detach(&self) {
        self.detached.set(true);
    }

    fn ensure_conversation_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.conversation_view.is_some() {
            return;
        }
        if let Some(new_agent) = self.new_agent.take() {
            PaseoConnection::global(cx).set_next_new_agent(new_agent);
        }
        self.set_conversation_view(
            build_conversation_view(
                self.agent_id.clone(),
                self.title.clone(),
                self.workspace.clone(),
                self.project.clone(),
                self.work_dirs.clone(),
                window,
                cx,
            ),
            window,
            cx,
        );
    }

    fn set_conversation_view(
        &mut self,
        conversation_view: Entity<ConversationView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._conversation_subscription =
            Some(cx.observe(&conversation_view, |_, _, cx| cx.notify()));
        // The tab may have been focused before its view existed.
        if self.focus_handle.is_focused(window) {
            let focus_handle = conversation_view.focus_handle(cx);
            cx.on_next_frame(window, move |_, window, cx| window.focus(&focus_handle, cx));
        }
        self.conversation_view = Some(conversation_view);
        cx.notify();
    }

    /// Reopens the session with the agent's whole history instead of the latest entries.
    fn load_full_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent_id) = self.agent_id(cx) else {
            return;
        };
        PaseoConnection::global(cx).request_full_history(&agent_id);
        self.agent_id = Some(agent_id.clone());
        let conversation_view = build_conversation_view(
            Some(agent_id),
            self.title.clone(),
            self.workspace.clone(),
            self.project.clone(),
            self.work_dirs.clone(),
            window,
            cx,
        );
        self.set_conversation_view(conversation_view, window, cx);
    }

    /// For the screenshot runner's consistency checks.
    #[cfg(any(test, feature = "test-support"))]
    pub fn conversation_view(&self) -> Option<&Entity<ConversationView>> {
        self.conversation_view.as_ref()
    }
}

fn build_conversation_view(
    agent_id: Option<String>,
    title: SharedString,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    work_dirs: PathList,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ConversationView> {
    let connection_store = cx.new(|cx| AgentConnectionStore::new(project.clone(), cx));
    cx.new(|cx| {
        ConversationView::new(
            Rc::new(PaseoAgentServer),
            connection_store,
            Agent::Custom {
                id: AgentId::new(PASEO_AGENT_ID),
            },
            agent_id.map(acp_v2::SessionId::new),
            None,
            Some(work_dirs),
            Some(title),
            None,
            workspace,
            project,
            None,
            AgentThreadSource::Sidebar,
            window,
            cx,
        )
    })
}

impl Item for PaseoAgentItem {
    type Event = ();

    fn include_in_nav_history() -> bool {
        false
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.conversation_view
            .as_ref()
            .and_then(|view| view.read(cx).active_thread())
            .and_then(|thread| thread.read(cx).thread.read(cx).title())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| self.title.clone())
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Sparkle))
    }

    /// Closing the tab archives the agent, as in Paseo's apps. A tab dragged to another pane is
    /// removed and added again in the same update, so the check waits a moment and only
    /// archives when the tab is gone.
    fn on_removed(&self, cx: &mut Context<Self>) {
        if self.detached.get() {
            return;
        }
        let Some(agent_id) = self.agent_id(cx) else {
            return;
        };
        let workspace = self.workspace.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::ZERO)
                .await;
            cx.update(|cx| {
                let still_open =
                    this.upgrade()
                        .zip(workspace.upgrade())
                        .is_some_and(|(item, workspace)| {
                            PaseoPanel::is_open(workspace.read(cx), item.entity_id(), cx)
                        });
                if !still_open {
                    crate::PaseoStore::global(cx).update(cx, |store, cx| {
                        store.close_items(vec![agent_id], Vec::new(), cx);
                    });
                }
            });
        })
        .detach();
    }
}

impl EventEmitter<()> for PaseoAgentItem {}

impl Focusable for PaseoAgentItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.conversation_view {
            Some(view) => view.read(cx).focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }
}

impl Render for PaseoAgentItem {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_conversation_view(window, cx);
        let truncated = self
            .agent_id(cx)
            .is_some_and(|agent_id| PaseoConnection::global(cx).history_truncated(&agent_id));
        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .when(truncated, |this| {
                this.child(
                    h_flex()
                        .flex_none()
                        .w_full()
                        .px_3()
                        .py_1()
                        .gap_2()
                        .border_b_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(
                            Label::new("Showing the latest part of this agent's history.")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Button::new("paseo-load-full-history", "Load Full History")
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.load_full_history(window, cx);
                                })),
                        ),
                )
            })
            .children(self.conversation_view.clone())
    }
}
