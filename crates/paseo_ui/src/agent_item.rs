use agent_client_protocol::v2::schema as acp_v2;
use agent_ui::{Agent, AgentConnectionStore, AgentThreadSource, ConversationView};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Window,
};
use project::AgentId;
use std::rc::Rc;
use ui::prelude::*;
use util::path_list::PathList;
use workspace::{Item, Workspace};

use crate::agent_session::{PASEO_AGENT_ID, PaseoAgentServer};

/// A Paseo agent session in a center-pane tab, rendered by Zed's thread UI.
pub struct PaseoAgentItem {
    /// `None` until a session started from this tab has been created in Paseo.
    agent_id: Option<String>,
    title: SharedString,
    conversation_view: Entity<ConversationView>,
}

impl PaseoAgentItem {
    /// Opens the agent's tab in `workspace`, or focuses it if it is already open.
    pub fn open(
        workspace: &mut Workspace,
        agent_id: Option<String>,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if let Some(agent_id) = &agent_id {
            let existing = workspace
                .items_of_type::<PaseoAgentItem>(cx)
                .find(|item| item.read(cx).agent_id.as_ref() == Some(agent_id));
            if let Some(existing) = existing {
                workspace.activate_item(&existing, true, true, window, cx);
                return;
            }
        }

        let project = workspace.project().clone();
        let work_dirs = PathList::new(&workspace.root_paths(cx));
        let connection_store = cx.new(|cx| AgentConnectionStore::new(project.clone(), cx));
        let weak_workspace = workspace.weak_handle();
        let conversation_view = cx.new(|cx| {
            ConversationView::new(
                Rc::new(PaseoAgentServer),
                connection_store,
                Agent::Custom {
                    id: AgentId::new(PASEO_AGENT_ID),
                },
                agent_id.clone().map(acp_v2::SessionId::new),
                None,
                Some(work_dirs),
                Some(title.clone()),
                None,
                weak_workspace,
                project,
                None,
                AgentThreadSource::Sidebar,
                window,
                cx,
            )
        });
        let item = cx.new(|_| Self {
            agent_id,
            title,
            conversation_view,
        });
        workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
    }
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
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().size_full().child(self.conversation_view.clone())
    }
}
