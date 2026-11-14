use futures::{StreamExt as _, channel::mpsc};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Subscription, Task, WeakEntity, Window,
};
use paseo_client::{
    Client,
    frames::{TerminalFrame, TerminalOpcode},
    protocol::{
        SubscribeTerminal, TerminalResizeIntent, TerminalRestore, TerminalRestoreMode,
        TerminalSize, TerminalStreamResize, UnsubscribeTerminal,
    },
};
use project::Project;
use settings::Settings as _;
use std::cell::Cell;
use terminal::terminal_settings::TerminalSettings;
use terminal::{ForwardedInput, Terminal, TerminalBounds, TerminalBuilder};
use terminal_view::TerminalView;
use ui::prelude::*;
use util::{ResultExt as _, paths::PathStyle};
use workspace::{Item, Workspace, WorkspaceId, item::ItemEvent, searchable::SearchableItemHandle};

use crate::{
    panel::PaseoPanel,
    store::{PaseoStore, PaseoStoreEvent},
};

/// A terminal owned by the Paseo daemon in a tab of the Paseo panel, rendered by Zed's terminal
/// view. Closing the tab kills the shell, as in Paseo's apps.
pub struct PaseoTerminalItem {
    terminal_id: String,
    workspace: WeakEntity<Workspace>,
    /// Set when pazedo removes the tab itself or the shell has exited. Only a tab the user
    /// closes kills the terminal.
    detached: Cell<bool>,
    name: SharedString,
    store: Entity<PaseoStore>,
    client: Option<Client>,
    terminal: Entity<Terminal>,
    terminal_view: Entity<TerminalView>,
    /// The daemon's slot for this terminal's frames on the current connection.
    slot: Option<u8>,
    size: Option<TerminalSize>,
    stream: Task<()>,
    _input: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl PaseoTerminalItem {
    /// Opens the terminal's tab in the workspace's Paseo panel, or focuses it if it is already
    /// open.
    pub fn open(
        workspace: &mut Workspace,
        terminal_id: String,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if let Some(existing) = Self::find(workspace, &terminal_id, cx) {
            PaseoPanel::show_item(workspace, Box::new(existing), window, cx);
            return;
        }
        let weak_workspace = workspace.weak_handle();
        let workspace_id = workspace.database_id();
        let project = workspace.project().downgrade();
        let item = cx.new(|cx| {
            Self::new(
                terminal_id,
                name,
                weak_workspace,
                workspace_id,
                project,
                window,
                cx,
            )
        });
        PaseoPanel::show_item(workspace, Box::new(item), window, cx);
    }

    pub(crate) fn find(workspace: &Workspace, terminal_id: &str, cx: &App) -> Option<Entity<Self>> {
        PaseoPanel::find_item::<Self>(workspace, cx, |item, _| item.terminal_id == terminal_id)
    }

    /// Marks the tab as removed by pazedo, so removing it does not kill the terminal.
    pub(crate) fn detach(&self) {
        self.detached.set(true);
    }

    pub(crate) fn new(
        terminal_id: String,
        name: SharedString,
        workspace: WeakEntity<Workspace>,
        workspace_id: Option<WorkspaceId>,
        project: WeakEntity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = TerminalSettings::get_global(cx).clone();
        let window_id = window.window_handle().window_id().as_u64();
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                settings.cursor_shape,
                settings.alternate_scroll,
                settings.max_scroll_history_lines,
                window_id,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        let (input_tx, mut input_rx) = mpsc::unbounded();
        terminal
            .update(cx, |terminal, _| terminal.forward_input(input_tx))
            .log_err();
        let terminal_view = cx.new(|cx| {
            TerminalView::new(
                terminal.clone(),
                workspace.clone(),
                workspace_id,
                project,
                window,
                cx,
            )
        });

        let input = cx.spawn(async move |this, cx| {
            while let Some(input) = input_rx.next().await {
                if this
                    .update(cx, |this, cx| this.forward_input(input, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        let store = PaseoStore::global(cx);
        let subscriptions = vec![
            cx.subscribe(&store, |this, _, event, cx| match event {
                PaseoStoreEvent::Connected => this.attach(cx),
                PaseoStoreEvent::Disconnected => this.slot = None,
                PaseoStoreEvent::TerminalStreamExit(exit)
                    if exit.terminal_id == this.terminal_id =>
                {
                    match &exit.error {
                        None => {
                            this.detach();
                            cx.emit(ItemEvent::CloseItem);
                        }
                        Some(error) => {
                            this.slot = None;
                            this.show_notice(&format!("Lost the terminal's stream: {error}"), cx);
                        }
                    }
                }
                _ => {}
            }),
            cx.subscribe(&terminal, |_, _, event: &terminal::Event, cx| {
                if let terminal::Event::TitleChanged | terminal::Event::BreadcrumbsChanged = event {
                    cx.emit(ItemEvent::UpdateTab);
                }
            }),
        ];

        // Attaching waits for the view's first layout (see `forward_input`).
        Self {
            terminal_id,
            workspace,
            detached: Cell::new(false),
            name,
            client: store.read(cx).client(),
            store,
            terminal,
            terminal_view,
            slot: None,
            size: None,
            stream: Task::ready(()),
            _input: input,
            _subscriptions: subscriptions,
        }
    }

    /// Subscribes to the terminal's stream on the current connection. The daemon answers with a
    /// restore of the screen and scrollback at this tab's size, then streams output.
    ///
    /// Waits until the view has a size: a restore written into the default-sized grid loses
    /// what does not fit when the grid is resized, and full-screen programs (vim, htop) live in
    /// the alternate screen, which is cut rather than reflowed.
    fn attach(&mut self, cx: &mut Context<Self>) {
        self.slot = None;
        if self.size.is_none() {
            return;
        }
        let Some(client) = self.client.clone().filter(Client::is_connected) else {
            return;
        };
        let request = SubscribeTerminal {
            terminal_id: self.terminal_id.clone(),
            restore: Some(TerminalRestore {
                mode: TerminalRestoreMode::FullSnapshot,
                scrollback_lines: None,
                size: self.size,
            }),
        };
        self.stream = cx.spawn(async move |this, cx| {
            let attached =
                client
                    .subscribe_terminal(&request)
                    .await
                    .and_then(|(response, frames)| {
                        let slot = response
                            .slot
                            .ok_or_else(|| anyhow::anyhow!("the daemon assigned no stream slot"))?;
                        Ok((slot, frames))
                    });
            let mut frames = match attached {
                Ok((slot, frames)) => {
                    if this
                        .update(cx, |this, _| {
                            this.slot = Some(slot);
                            this.send_size();
                        })
                        .is_err()
                    {
                        return;
                    }
                    frames
                }
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.show_notice(
                            &format!("Could not attach to the terminal: {error:#}"),
                            cx,
                        )
                    })
                    .ok();
                    return;
                }
            };
            while let Some(frame) = frames.next().await {
                if this
                    .update(cx, |this, cx| this.apply_frame(frame, cx))
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    pub(crate) fn apply_frame(&mut self, frame: TerminalFrame, cx: &mut Context<Self>) {
        match frame.opcode {
            TerminalOpcode::Output => self.terminal.update(cx, |terminal, cx| {
                terminal.write_raw_output(&frame.payload, cx);
            }),
            // Paseo renders the restore with xterm's serializer, so it is ANSI that rebuilds the
            // screen and scrollback from scratch.
            TerminalOpcode::Restore => self
                .terminal
                .update(cx, |terminal, cx| {
                    terminal.replace_display_output(&frame.payload, cx)
                })
                .log_err()
                .unwrap_or_default(),
            other => log::debug!("ignoring Paseo terminal frame {other:?}"),
        }
    }

    fn forward_input(&mut self, input: ForwardedInput, cx: &mut Context<Self>) {
        match input {
            ForwardedInput::Bytes(bytes) => {
                let (Some(client), Some(slot)) = (&self.client, self.slot) else {
                    return;
                };
                client
                    .send_terminal_frame(&TerminalFrame {
                        opcode: TerminalOpcode::Input,
                        slot,
                        payload: bytes,
                    })
                    .log_err();
            }
            ForwardedInput::Resize(bounds) => {
                let first_layout = self.size.is_none();
                self.size = Some(terminal_size(&bounds));
                if first_layout {
                    self.attach(cx);
                } else {
                    self.send_size();
                }
            }
        }
    }

    /// Resizing in this tab takes over the terminal's size from other clients, as typing into a
    /// terminal in Paseo's apps does.
    fn send_size(&self) {
        let (Some(client), Some(slot), Some(size)) = (&self.client, self.slot, self.size) else {
            return;
        };
        let resize = TerminalStreamResize {
            rows: size.rows,
            cols: size.cols,
            intent: Some(TerminalResizeIntent::Claim),
        };
        let Some(payload) = serde_json::to_vec(&resize).log_err() else {
            return;
        };
        client
            .send_terminal_frame(&TerminalFrame {
                opcode: TerminalOpcode::Resize,
                slot,
                payload,
            })
            .log_err();
    }

    fn show_notice(&self, notice: &str, cx: &mut Context<Self>) {
        let line = format!("\r\n\x1b[2m[{notice}]\x1b[0m\r\n");
        self.terminal.update(cx, |terminal, cx| {
            terminal.write_raw_output(line.as_bytes(), cx);
        });
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn terminal(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    pub fn terminal_id(&self) -> &str {
        &self.terminal_id
    }
}

fn terminal_size(bounds: &TerminalBounds) -> TerminalSize {
    let clamp = |count: usize| u16::try_from(count).unwrap_or(u16::MAX).max(1);
    TerminalSize {
        rows: clamp(bounds.num_lines()),
        cols: clamp(bounds.num_columns()),
    }
}

impl Drop for PaseoTerminalItem {
    fn drop(&mut self) {
        if let (Some(client), Some(_)) = (&self.client, self.slot) {
            client
                .notify(&UnsubscribeTerminal {
                    terminal_id: self.terminal_id.clone(),
                })
                .log_err();
        }
    }
}

impl Item for PaseoTerminalItem {
    type Event = ItemEvent;

    fn include_in_nav_history() -> bool {
        false
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        let shell_title = self.terminal.read(cx).breadcrumb_text.trim();
        if !shell_title.is_empty() {
            return shell_title.to_string().into();
        }
        self.store
            .read(cx)
            .terminal(&self.terminal_id)
            .and_then(|terminal| terminal.title.clone())
            .filter(|title| !title.is_empty())
            .map(SharedString::from)
            .unwrap_or_else(|| self.name.clone())
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Terminal))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.name.clone())
    }

    fn as_searchable(
        &self,
        _handle: &Entity<Self>,
        _cx: &App,
    ) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.terminal_view.clone()))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    /// Closing the tab kills the terminal, as in Paseo's apps. A tab dragged to another pane is
    /// removed and added again in the same update, so the check waits a moment and only kills
    /// the terminal when the tab is gone.
    fn on_removed(&self, cx: &mut Context<Self>) {
        if self.detached.get() {
            return;
        }
        let terminal_id = self.terminal_id.clone();
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
                    PaseoStore::global(cx).update(cx, |store, cx| {
                        store.close_items(Vec::new(), vec![terminal_id], cx);
                    });
                }
            });
        })
        .detach();
    }
}

impl EventEmitter<ItemEvent> for PaseoTerminalItem {}

impl Focusable for PaseoTerminalItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.terminal_view.read(cx).focus_handle(cx)
    }
}

impl Render for PaseoTerminalItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.terminal_view.clone())
    }
}
