//! The work an agent left running in the background (Paseo 0.16: shells, monitors), shown above
//! its message box with a Stop button per item, as Paseo's apps show it.

use std::rc::Rc;

use agent_settings::AgentSettings;
use agent_ui::ThreadComposerBar;
use chrono::{DateTime, Local};
use collections::HashMap;
use gpui::{App, Context, Entity, SharedString, Subscription, Window, div, prelude::*};
use paseo_client::protocol::BackgroundWorkItem;
use settings::Settings as _;
use ui::{Disclosure, prelude::*};

use crate::agent_session::PaseoConnection;
use crate::store::PaseoStore;

pub(crate) fn init(cx: &mut App) {
    cx.set_global(ThreadComposerBar(Rc::new(|thread, window, cx| {
        let thread = thread.read(cx);
        let connection = thread
            .connection()
            .clone()
            .into_any()
            .downcast::<PaseoConnection>()
            .ok()?;
        let agent_id = thread.session_id().0.to_string();
        let store = connection.store().clone();
        let bar = window.use_keyed_state(
            SharedString::from(format!("paseo-background-work-{agent_id}")),
            cx,
            |_, cx| BackgroundWorkBar::new(store, agent_id, cx),
        );
        Some(bar.into_any_element())
    })));
}

enum StopState {
    Stopping,
    Failed(SharedString),
}

struct BackgroundWorkBar {
    store: Entity<PaseoStore>,
    agent_id: String,
    expanded: bool,
    /// By item id, until the item leaves the list.
    stops: HashMap<String, StopState>,
    _store_subscription: Subscription,
}

impl BackgroundWorkBar {
    fn new(store: Entity<PaseoStore>, agent_id: String, cx: &mut Context<Self>) -> Self {
        let store_subscription = cx.observe(&store, |this, store, cx| {
            if let Some(items) = store.read(cx).background_work(&this.agent_id) {
                this.stops
                    .retain(|id, _| items.iter().any(|item| &item.id == id));
            }
            cx.notify();
        });
        Self {
            store,
            agent_id,
            expanded: false,
            stops: HashMap::default(),
            _store_subscription: store_subscription,
        }
    }

    fn stop(&mut self, item: BackgroundWorkItem, cx: &mut Context<Self>) {
        self.stops.insert(item.id.clone(), StopState::Stopping);
        let stop = self
            .store
            .read(cx)
            .stop_background_work(&self.agent_id, &item, cx);
        cx.spawn(async move |this, cx| {
            let result = stop.await;
            this.update(cx, |this, cx| {
                // A stopped item stays "Stopping…" until the daemon's update removes it.
                if let Err(error) = result {
                    log::error!(
                        "failed to stop Paseo background work {}: {error:#}",
                        item.id
                    );
                    this.stops
                        .insert(item.id, StopState::Failed(format!("{error:#}").into()));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn render_item(
        &self,
        item: &BackgroundWorkItem,
        can_stop: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let text = item_text(item);
        let stop_state = self.stops.get(&item.id);
        let stopping = matches!(stop_state, Some(StopState::Stopping));
        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .justify_between()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .child(Label::new(text.title).size(LabelSize::Small).truncate())
                    .when_some(text.command, |this, command| {
                        this.child(
                            Label::new(command)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .buffer_font(cx)
                                .truncate(),
                        )
                    })
                    .child(
                        Label::new(text.facts)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .when_some(
                        match stop_state {
                            Some(StopState::Failed(error)) => Some(error.clone()),
                            _ => None,
                        },
                        |this, error| {
                            this.child(
                                Label::new(error)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Error),
                            )
                        },
                    ),
            )
            .when(
                can_stop && item.can_stop && item.stop_token.is_some(),
                |this| {
                    this.child(
                        Button::new(
                            SharedString::from(format!("paseo-stop-background-work-{}", item.id)),
                            if stopping { "Stopping…" } else { "Stop" },
                        )
                        .label_size(LabelSize::Small)
                        .disabled(stopping)
                        .on_click(cx.listener({
                            let item = item.clone();
                            move |this, _, _, cx| this.stop(item.clone(), cx)
                        })),
                    )
                },
            )
    }
}

impl Render for BackgroundWorkBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let count = store.background_work_count(&self.agent_id);
        if count == 0 {
            return div().into_any_element();
        }
        let items = store.background_work(&self.agent_id).map(<[_]>::to_vec);
        let can_stop = store.stops_background_work();
        if items.is_none() {
            let store = self.store.clone();
            let agent_id = self.agent_id.clone();
            cx.defer(move |cx| {
                store.update(cx, |store, cx| store.load_background_work(&agent_id, cx));
            });
        }

        let title: SharedString = if count == 1 {
            "1 Background Task".into()
        } else {
            format!("{count} Background Tasks").into()
        };
        let colors = cx.theme().colors();
        let max_content_width = AgentSettings::get_global(cx).max_content_width;
        // Zed's own bars above the message box (edits, plan, queue) look like this.
        h_flex()
            .w_full()
            .px_2()
            .justify_center()
            .child(
                v_flex()
                    .when_some(max_content_width, |this, max_width| {
                        this.flex_basis(max_width)
                    })
                    .when(max_content_width.is_none(), |this| this.w_full())
                    .flex_shrink_1()
                    .flex_grow_0()
                    .max_w_full()
                    .bg(colors
                        .editor_background
                        .blend(colors.element_selected.opacity(0.3)))
                    .border_1()
                    .border_b_0()
                    .border_color(colors.border)
                    .rounded_t_md()
                    .child(
                        h_flex()
                            .id("paseo-background-work-summary")
                            .p_1()
                            .gap_1()
                            .w_full()
                            .cursor_pointer()
                            .child(Disclosure::new(
                                "paseo-background-work-disclosure",
                                self.expanded,
                            ))
                            .child(Label::new(title).size(LabelSize::Small).color(Color::Muted))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.expanded = !this.expanded;
                                cx.notify();
                            })),
                    )
                    .when(self.expanded, |this| match items {
                        Some(items) => this.children(
                            items
                                .iter()
                                .map(|item| self.render_item(item, can_stop, cx).into_any_element())
                                .collect::<Vec<_>>(),
                        ),
                        None => this.child(
                            div().px_2().py_1().child(
                                Label::new("Loading…")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                        ),
                    }),
            )
            .into_any_element()
    }
}

struct ItemText {
    title: SharedString,
    /// Shown when it is not the title already.
    command: Option<SharedString>,
    /// The kind and when it started.
    facts: SharedString,
}

/// As Paseo's apps word it: the description, else the command, else the kind.
fn item_text(item: &BackgroundWorkItem) -> ItemText {
    let kind = match item.kind.as_str() {
        "shell" => "Shell",
        "monitor" => "Monitor",
        _ => "Background task",
    };
    let title = item
        .description
        .clone()
        .or_else(|| item.command.clone())
        .unwrap_or_else(|| kind.to_string());
    let command = item
        .command
        .clone()
        .filter(|command| *command != title)
        .map(Into::into);
    let facts = match DateTime::parse_from_rfc3339(&item.started_at) {
        Ok(started_at) => format!(
            "{kind} · since {}",
            started_at.with_timezone(&Local).format("%H:%M")
        ),
        Err(_) => kind.to_string(),
    };
    ItemText {
        title: title.into(),
        command,
        facts: facts.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(value: serde_json::Value) -> BackgroundWorkItem {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn items_read_like_in_paseo() {
        let shell = item_text(&item(serde_json::json!({
            "id": "b1", "kind": "shell", "description": "Run the dev server",
            "command": "npm run dev", "startedAt": "not a date", "canStop": true, "stopToken": "t",
        })));
        assert_eq!(shell.title.as_ref(), "Run the dev server");
        assert_eq!(shell.command.as_deref(), Some("npm run dev"));
        assert_eq!(shell.facts.as_ref(), "Shell");

        let command_only = item_text(&item(serde_json::json!({
            "id": "b2", "kind": "shell", "description": null, "command": "sleep 600",
            "startedAt": "2026-11-21T10:00:00Z",
        })));
        assert_eq!(command_only.title.as_ref(), "sleep 600");
        assert_eq!(
            command_only.command, None,
            "the command is the title already"
        );
        assert!(command_only.facts.starts_with("Shell · since "));

        let unknown = item_text(&item(serde_json::json!({
            "id": "b3", "kind": "subagent", "description": null, "startedAt": "",
        })));
        assert_eq!(unknown.title.as_ref(), "Background task");
    }
}
