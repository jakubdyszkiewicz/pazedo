use agent_client_protocol::v2::schema as acp_v2;
use agent_ui::{Agent, AgentConnectionStore, AgentThreadSource, ConversationView};
use gpui::{
    Action, App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Render, SharedString, Subscription, Task, WeakEntity, Window,
};
use project::{AgentId, Project};
use std::{cell::Cell, rc::Rc};
use ui::prelude::*;
use util::path_list::PathList;
use workspace::{Item, Workspace, notifications::DetachAndPromptErr as _};

use crate::{
    PaseoStore, PaseoStoreEvent,
    agent_session::{NewAgent, PASEO_AGENT_ID, PaseoAgentServer, PaseoConnection},
    panel::PaseoPanel,
    rename_prompt::RenamePrompt,
    store::ConnectionStatus,
};
use paseo_client::protocol::AgentStatus;

/// Copies an agent's id, e.g. for the `paseo` CLI. From the tab's context menu.
#[derive(Clone, PartialEq, Action)]
#[action(namespace = paseo_agent, no_json, no_register)]
pub struct CopyAgentId {
    pub agent_id: String,
}

/// Renames an agent. From the tab's context menu.
#[derive(Clone, PartialEq, Action)]
#[action(namespace = paseo_agent, no_json, no_register)]
pub struct RenameAgent {
    pub agent_id: String,
}

/// The tab menu's actions are handled app-wide: the menu dispatches them to the tab, which is
/// not rendered unless it is the active one, and they then reach only the window's root.
pub(crate) fn init(cx: &mut App) {
    cx.on_action(|action: &CopyAgentId, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(action.agent_id.clone()));
    });
    cx.on_action(|action: &RenameAgent, cx| {
        let agent_id = action.agent_id.clone();
        // The action arrives while its window is being updated.
        cx.defer(move |cx| {
            crate::rename_prompt::with_active_workspace(cx, |workspace, window, cx| {
                if let Some(item) = PaseoAgentItem::find(workspace.read(cx), &agent_id, cx) {
                    item.update(cx, |item, cx| item.rename(window, cx));
                }
            });
        });
    });
}

/// The icon of an agent's provider, from its id or label: "contains", not an exact match, so
/// custom providers such as "Claude (Work)" get theirs too.
pub(crate) fn provider_icon(names: &[&str]) -> IconName {
    let names: Vec<String> = names.iter().map(|name| name.to_lowercase()).collect();
    let mentions = |needle: &str| names.iter().any(|name| name.contains(needle));
    if mentions("claude") || mentions("anthropic") {
        IconName::AiClaude
    } else if mentions("codex") || mentions("openai") {
        IconName::AiOpenAi
    } else if mentions("opencode") {
        IconName::AiOpenCode
    } else if mentions("copilot") {
        IconName::Copilot
    } else if mentions("gemini") {
        IconName::AiGemini
    } else {
        IconName::Sparkle
    }
}

/// A Paseo agent session in a tab of the Paseo panel, rendered by Zed's thread UI.
pub struct PaseoAgentItem {
    /// The agent's daemon.
    store: Entity<PaseoStore>,
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
    /// While a closed agent is being reloaded.
    reloading: Option<Task<()>>,
    /// Set when the session lost its daemon connection (or the daemon replaced the agent's
    /// history). The view reloads once the daemon is connected again and the tab is shown,
    /// instead of waiting for the user to press Retry, e.g. after the laptop slept.
    reload_when_connected: bool,
    /// The agent's title as the daemon last reported it.
    daemon_title: Option<String>,
    _conversation_subscription: Option<Subscription>,
    _store_subscriptions: [Subscription; 2],
}

impl PaseoAgentItem {
    /// Opens the agent's tab in the workspace's Paseo panel, or focuses it if it is already
    /// open. With no agent id it starts a new agent with `new_agent`'s provider and model.
    pub fn open(
        workspace: &mut Workspace,
        store: Entity<PaseoStore>,
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
                store,
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
        store: Entity<PaseoStore>,
        agent_id: Option<String>,
        new_agent: Option<NewAgent>,
        title: SharedString,
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut Context<Self>,
    ) -> Self {
        // Rendering marks the agent as seen; this re-renders a shown tab when its agent
        // finishes, so that it does not show up as unread while the user is looking at it.
        // It also re-renders the tab when the agent is renamed.
        let store_subscription = cx.observe(&store, |this, store, cx| {
            let agent_id = this.agent_id(cx);
            let title = agent_id
                .as_ref()
                .and_then(|agent_id| store.read(cx).agent(agent_id)?.title.clone());
            let renamed = title != this.daemon_title;
            this.daemon_title = title;
            if renamed
                || agent_id.is_some_and(|agent_id| store.read(cx).has_unseen_attention(&agent_id))
            {
                cx.notify();
            }
        });
        let events_subscription = cx.subscribe(&store, |this, _, event, cx| match event {
            PaseoStoreEvent::Disconnected => {
                if this.conversation_view.is_some() {
                    this.reload_when_connected = true;
                }
            }
            PaseoStoreEvent::AgentTimelineReplacement(replacement)
                if this.agent_id(cx).as_deref() == Some(replacement.agent_id.as_str()) =>
            {
                this.reload_when_connected = true;
                cx.notify();
            }
            PaseoStoreEvent::Connected if this.reload_when_connected => cx.notify(),
            _ => {}
        });
        Self {
            store,
            agent_id,
            new_agent,
            title,
            workspace,
            project,
            work_dirs,
            conversation_view: None,
            focus_handle: cx.focus_handle(),
            detached: Cell::new(false),
            reloading: None,
            reload_when_connected: false,
            daemon_title: None,
            _conversation_subscription: None,
            _store_subscriptions: [store_subscription, events_subscription],
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

    /// Reopens a session that lost its connection once the daemon is back.
    fn reload_if_reconnected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.reload_when_connected
            || !matches!(self.store.read(cx).status(), ConnectionStatus::Connected(_))
        {
            return;
        }
        // A tab that started an agent whose id never became known has nothing to reopen; it
        // keeps Zed's Retry, which starts the agent again.
        let Some(agent_id) = self.agent_id.clone() else {
            return;
        };
        self.reload_when_connected = false;
        let conversation_view = build_conversation_view(
            self.store.clone(),
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

    fn ensure_conversation_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.conversation_view.is_some() {
            return;
        }
        if let Some(new_agent) = self.new_agent.take() {
            PaseoConnection::for_store(&self.store, cx).set_next_new_agent(new_agent);
        }
        self.set_conversation_view(
            build_conversation_view(
                self.store.clone(),
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
        PaseoConnection::for_store(&self.store, cx).request_full_history(&agent_id);
        self.agent_id = Some(agent_id.clone());
        let conversation_view = build_conversation_view(
            self.store.clone(),
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

    /// Resumes a closed agent (its provider session ended, e.g. when the daemon restarted), which
    /// also brings back its history, then reopens the session; "Reload agent" in Paseo's apps.
    fn reload_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent_id) = self.agent_id(cx) else {
            return;
        };
        let refresh = self.store.read(cx).refresh_agent(agent_id.clone(), cx);
        self.reloading = Some(cx.spawn_in(window, async move |this, cx| {
            let result = refresh.await;
            this.update_in(cx, |this, window, cx| {
                this.reloading = None;
                match result {
                    Ok(()) => {
                        let conversation_view = build_conversation_view(
                            this.store.clone(),
                            Some(agent_id),
                            this.title.clone(),
                            this.workspace.clone(),
                            this.project.clone(),
                            this.work_dirs.clone(),
                            window,
                            cx,
                        );
                        this.set_conversation_view(conversation_view, window, cx);
                    }
                    Err(error) => {
                        log::error!("could not reload Paseo agent {agent_id}: {error:#}");
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_closed_notice(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let reloading = self.reloading.is_some();
        h_flex()
            .flex_none()
            .w_full()
            .px_3()
            .py_1()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Label::new(
                    "This agent's session is closed. Reload it to see its history and continue.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                Button::new(
                    "paseo-reload-agent",
                    if reloading {
                        "Reloading…"
                    } else {
                        "Reload Agent"
                    },
                )
                .label_size(LabelSize::Small)
                .disabled(reloading)
                .on_click(cx.listener(|this, _, window, cx| this.reload_agent(window, cx))),
            )
    }

    /// For the screenshot runner's consistency checks.
    #[cfg(any(test, feature = "test-support"))]
    pub fn conversation_view(&self) -> Option<&Entity<ConversationView>> {
        self.conversation_view.as_ref()
    }
}

fn build_conversation_view(
    store: Entity<PaseoStore>,
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
            Rc::new(PaseoAgentServer { store }),
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

impl PaseoAgentItem {
    fn rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent_id) = self.agent_id(cx) else {
            return;
        };
        let current_name = self.tab_content_text(0, cx).to_string();
        let store = self.store.clone();
        RenamePrompt::open(
            &self.workspace,
            "Rename Agent",
            current_name,
            move |name, window, cx| {
                store
                    .update(cx, |store, cx| store.rename_agent(agent_id, name, cx))
                    .detach_and_prompt_err(
                        "Could not rename the agent",
                        window,
                        cx,
                        |error, _, _| Some(format!("{error:#}")),
                    );
            },
            window,
            cx,
        );
    }
}

impl Item for PaseoAgentItem {
    type Event = ();

    fn include_in_nav_history() -> bool {
        false
    }

    /// The daemon's title first: the thread only reads it when the session opens, so a
    /// rename would not show otherwise.
    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.agent_id(cx)
            .and_then(|agent_id| self.store.read(cx).agent(&agent_id)?.title.clone())
            .map(SharedString::from)
            .or_else(|| {
                self.conversation_view
                    .as_ref()
                    .and_then(|view| view.read(cx).active_thread())
                    .and_then(|thread| thread.read(cx).thread.read(cx).title())
            })
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| self.title.clone())
    }

    fn tab_icon(&self, _window: &Window, cx: &App) -> Option<Icon> {
        let store = self.store.read(cx);
        let provider = self
            .agent_id(cx)
            .and_then(|agent_id| Some(store.agent(&agent_id)?.provider.clone()))
            .or_else(|| self.new_agent.as_ref().map(|agent| agent.provider.clone()))?;
        let label = store
            .providers()
            .iter()
            .find(|entry| entry.provider == provider)
            .and_then(|entry| entry.label.clone());
        let mut names = vec![provider.as_str()];
        names.extend(label.as_deref());
        Some(Icon::new(provider_icon(&names)))
    }

    fn tab_extra_context_menu_actions(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<(SharedString, Box<dyn Action>)> {
        let Some(agent_id) = self.agent_id(cx) else {
            return Vec::new();
        };
        vec![
            (
                "Copy Agent ID".into(),
                Box::new(CopyAgentId {
                    agent_id: agent_id.clone(),
                }),
            ),
            ("Rename…".into(), Box::new(RenameAgent { agent_id })),
        ]
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
        let store = self.store.clone();
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
                    store.update(cx, |store, cx| {
                        store.close_items(vec![agent_id], Vec::new(), cx);
                    });
                }
            });
        })
        .detach();
    }
}

impl EventEmitter<()> for PaseoAgentItem {}

/// Focusing the tab (the panel's toggle, switching tabs) lands in the message box, as activating
/// a thread does in Zed's agent panel.
impl Focusable for PaseoAgentItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        let Some(view) = &self.conversation_view else {
            return self.focus_handle.clone();
        };
        match view.read(cx).active_thread() {
            Some(thread) => thread.read(cx).message_editor.focus_handle(cx),
            None => view.read(cx).focus_handle(cx),
        }
    }
}

impl Render for PaseoAgentItem {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_conversation_view(window, cx);
        let agent_id = self.agent_id(cx);
        // A tab that started an agent keeps its id, which the view forgets when its session
        // is lost and which reopening the agent needs.
        if self.agent_id.is_none() && agent_id.is_some() {
            self.agent_id = agent_id.clone();
        }
        self.reload_if_reconnected(window, cx);
        // Only the active tab of a shown panel renders, so this is the user seeing the agent.
        if window.is_window_active()
            && let Some(agent_id) = agent_id.clone()
            && self.store.read(cx).has_unseen_attention(&agent_id)
        {
            let store = self.store.clone();
            cx.defer(move |cx| {
                store.update(cx, |store, cx| store.clear_agent_attention(&agent_id, cx));
            });
        }
        let truncated = agent_id.as_ref().is_some_and(|agent_id| {
            PaseoConnection::for_store(&self.store, cx).history_truncated(agent_id)
        });
        let closed = agent_id.as_ref().is_some_and(|agent_id| {
            self.store
                .read(cx)
                .agent(agent_id)
                .is_some_and(|agent| agent.status == AgentStatus::Closed)
        });
        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .when(closed, |this| this.child(self.render_closed_notice(cx)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_icons_match_by_containing_the_name() {
        assert_eq!(provider_icon(&["claude"]), IconName::AiClaude);
        assert_eq!(
            provider_icon(&["work-claude", "Claude (Work)"]),
            IconName::AiClaude
        );
        assert_eq!(provider_icon(&["codex"]), IconName::AiOpenAi);
        assert_eq!(
            provider_icon(&["custom-1", "Codex (work)"]),
            IconName::AiOpenAi,
            "the label counts too"
        );
        assert_eq!(provider_icon(&["opencode"]), IconName::AiOpenCode);
        assert_eq!(provider_icon(&["OpenCode Go"]), IconName::AiOpenCode);
        assert_eq!(provider_icon(&["mock"]), IconName::Sparkle);
    }
}
