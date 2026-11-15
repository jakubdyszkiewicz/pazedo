use gpui::{App, Context, DismissEvent, Entity, SharedString, Task, Window};
use picker::{Picker, PickerDelegate};
use std::{rc::Rc, sync::Arc, time::Duration};
use ui::{ListItem, ListItemSpacing, prelude::*};
use workspace::Workspace;

use crate::PaseoStore;

type OnConfirm = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// Picks a folder on the daemon's machine to add as a project, for a daemon on another
/// machine, where the native folder dialog cannot look. Suggestions come from the daemon as the
/// path or name is typed.
pub(crate) struct AddProjectDelegate {
    store: Entity<PaseoStore>,
    host_name: SharedString,
    matches: Vec<String>,
    selected_index: usize,
    error: Option<SharedString>,
    on_confirm: OnConfirm,
}

/// Opens the picker in `workspace`; `on_confirm` adds the picked folder.
pub(crate) fn open_add_project_picker(
    workspace: &mut Workspace,
    store: Entity<PaseoStore>,
    host_name: SharedString,
    on_confirm: impl Fn(String, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.toggle_modal(window, cx, move |window, cx| {
        let delegate = AddProjectDelegate {
            store,
            host_name,
            matches: Vec::new(),
            selected_index: 0,
            error: None,
            on_confirm: Rc::new(on_confirm),
        };
        Picker::uniform_list(delegate, window, cx)
    });
}

impl PickerDelegate for AddProjectDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "paseo-add-project"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        format!(
            "Folder on {} to add as a project, e.g. ~/dev/…",
            self.host_name
        )
        .into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(
            self.error
                .clone()
                .unwrap_or_else(|| "No matching folders".into()),
        )
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_string();
        cx.spawn_in(window, async move |picker, cx| {
            // The picker drops this task on the next keystroke, so only a pause asks.
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            let suggestions = picker
                .update(cx, |picker, cx| {
                    let query = if query.is_empty() {
                        "~/".to_string()
                    } else {
                        query.clone()
                    };
                    picker
                        .delegate
                        .store
                        .read(cx)
                        .directory_suggestions(query, cx)
                })
                .ok();
            let Some(suggestions) = suggestions else {
                return;
            };
            let result = suggestions.await;
            picker
                .update(cx, |picker, cx| {
                    let delegate = &mut picker.delegate;
                    match result {
                        Ok(mut directories) => {
                            // A typed absolute path is offered as is, even before it exists
                            // in the suggestions (e.g. a folder the daemon does not list).
                            let typed = query.trim_end_matches('/');
                            if typed.starts_with('/')
                                && !directories.iter().any(|directory| directory == typed)
                            {
                                directories.insert(0, typed.to_string());
                            }
                            delegate.matches = directories;
                            delegate.error = None;
                        }
                        Err(error) => {
                            delegate.matches.clear();
                            delegate.error = Some(format!("{error:#}").into());
                        }
                    }
                    delegate.selected_index = 0;
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(path) = self.matches.get(self.selected_index).cloned() {
            (self.on_confirm)(path, window, cx);
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let path = self.matches.get(ix)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(
                    Icon::new(IconName::Folder)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(Label::new(path.clone())),
        )
    }
}
