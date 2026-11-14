use agent_client_protocol::v2::schema as acp_v2;
use agent_ui::{Agent, AgentConnectionStore, AgentThreadSource, ConversationView};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Subscription, WeakEntity, Window,
};
use project::{AgentId, Project};
use std::rc::Rc;
use ui::prelude::*;
use util::path_list::PathList;
use workspace::{Item, Workspace};

use crate::{
    agent_session::{NewAgent, PASEO_AGENT_ID, PaseoAgentServer, PaseoConnection},
    panel::PaseoPanel,
};

/// A Paseo agent session in a center-pane tab, rendered by Zed's thread UI.
pub struct PaseoAgentItem {
    /// `None` for a tab that started a new agent; its id is then the thread's session id.
    agent_id: Option<String>,
    title: SharedString,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    work_dirs: PathList,
    conversation_view: Entity<ConversationView>,
    _conversation_subscription: Subscription,
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
        if let Some(agent_id) = &agent_id {
            let existing = PaseoPanel::find_item::<PaseoAgentItem>(workspace, cx, |item, cx| {
                item.agent_id(cx).as_ref() == Some(agent_id)
            });
            if let Some(existing) = existing {
                PaseoPanel::show_item(workspace, Box::new(existing), window, cx);
                return;
            }
        } else {
            PaseoConnection::global(cx)
                .set_next_new_agent(new_agent.unwrap_or_else(NewAgent::from_env));
        }

        let project = workspace.project().clone();
        let work_dirs = PathList::new(&workspace.root_paths(cx));
        let weak_workspace = workspace.weak_handle();
        let item = cx.new(|cx| {
            let conversation_view = build_conversation_view(
                agent_id.clone(),
                title.clone(),
                weak_workspace.clone(),
                project.clone(),
                work_dirs.clone(),
                window,
                cx,
            );
            Self {
                agent_id,
                title,
                workspace: weak_workspace,
                project,
                work_dirs,
                _conversation_subscription: cx.observe(&conversation_view, |_, _, cx| cx.notify()),
                conversation_view,
            }
        });
        PaseoPanel::show_item(workspace, Box::new(item), window, cx);
    }

    fn agent_id(&self, cx: &App) -> Option<String> {
        self.agent_id.clone().or_else(|| {
            self.conversation_view
                .read(cx)
                .active_thread()
                .map(|thread| thread.read(cx).thread.read(cx).session_id().0.to_string())
        })
    }

    /// Reopens the session with the agent's whole history instead of the latest entries.
    fn load_full_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent_id) = self.agent_id(cx) else {
            return;
        };
        PaseoConnection::global(cx).request_full_history(&agent_id);
        self.agent_id = Some(agent_id.clone());
        self.conversation_view = build_conversation_view(
            Some(agent_id),
            self.title.clone(),
            self.workspace.clone(),
            self.project.clone(),
            self.work_dirs.clone(),
            window,
            cx,
        );
        self._conversation_subscription =
            cx.observe(&self.conversation_view, |_, _, cx| cx.notify());
        cx.notify();
    }

    /// For the screenshot runner's consistency checks.
    #[cfg(any(test, feature = "test-support"))]
    pub fn conversation_view(&self) -> &Entity<ConversationView> {
        &self.conversation_view
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
            .read(cx)
            .active_thread()
            .and_then(|thread| thread.read(cx).thread.read(cx).title())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| self.title.clone())
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Sparkle))
    }
}

impl EventEmitter<()> for PaseoAgentItem {}

impl Focusable for PaseoAgentItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.conversation_view.read(cx).focus_handle(cx)
    }
}

impl Render for PaseoAgentItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let truncated = self
            .agent_id(cx)
            .is_some_and(|agent_id| PaseoConnection::global(cx).history_truncated(&agent_id));
        v_flex()
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
            .child(self.conversation_view.clone())
    }
}
