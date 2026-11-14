use gpui::{App, Context, DismissEvent, SharedString, Task, Window};
use picker::{Picker, PickerDelegate};
use std::{rc::Rc, sync::Arc};
use ui::{ListItem, ListItemSpacing, prelude::*};
use workspace::Workspace;

use crate::PaseoStore;

/// Where a new workspace of a project lives, as in Paseo's new-workspace dialog.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NewWorkspaceChoice {
    /// The project's own checkout.
    Local,
    /// A new git worktree on a new branch off `base_branch`; the daemon names the branch.
    Worktree { base_branch: String },
}

type OnConfirm = Rc<dyn Fn(NewWorkspaceChoice, &mut Window, &mut App)>;

/// Picks where a new workspace of a project lives: its checkout, or a new worktree off one of
/// its branches (most recently committed first, searchable).
pub(crate) struct NewWorkspaceDelegate {
    project_name: SharedString,
    /// `None` until the daemon has listed them.
    branches: Option<Vec<String>>,
    matches: Vec<NewWorkspaceChoice>,
    selected_index: usize,
    on_confirm: OnConfirm,
}

/// Opens the picker in `workspace`; `on_confirm` creates the workspace.
pub(crate) fn open_new_workspace_picker(
    workspace: &mut Workspace,
    project_name: SharedString,
    project_root: String,
    on_confirm: impl Fn(NewWorkspaceChoice, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let branches = PaseoStore::global(cx)
        .read(cx)
        .branch_suggestions(project_root, cx);
    workspace.toggle_modal(window, cx, move |window, cx| {
        let delegate = NewWorkspaceDelegate {
            project_name,
            branches: None,
            matches: vec![NewWorkspaceChoice::Local],
            selected_index: 0,
            on_confirm: Rc::new(on_confirm),
        };
        cx.spawn_in(window, async move |picker, cx| {
            let branches = match branches.await {
                Ok(branches) => branches,
                Err(error) => {
                    log::warn!("{error:#}");
                    Vec::new()
                }
            };
            picker
                .update_in(
                    cx,
                    |picker: &mut Picker<NewWorkspaceDelegate>, window, cx| {
                        picker.delegate.branches = Some(branches);
                        picker.refresh(window, cx);
                    },
                )
                .ok();
        })
        .detach();
        Picker::uniform_list(delegate, window, cx)
    });
}

impl PickerDelegate for NewWorkspaceDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "paseo-new-workspace"
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

    fn separators_after_indices(&self) -> Vec<usize> {
        match self.matches.first() {
            Some(NewWorkspaceChoice::Local) if self.matches.len() > 1 => vec![0],
            _ => Vec::new(),
        }
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        format!(
            "New workspace in {}: local or a worktree off a branch…",
            self.project_name
        )
        .into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(match self.branches {
            None => "Loading branches…".into(),
            Some(_) => "No branch matches".into(),
        })
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_lowercase();
        let mut matches = Vec::new();
        if query.is_empty() || "local checkout".contains(&query) {
            matches.push(NewWorkspaceChoice::Local);
        }
        matches.extend(
            self.branches
                .iter()
                .flatten()
                .filter(|branch| branch.to_lowercase().contains(&query))
                .map(|branch| NewWorkspaceChoice::Worktree {
                    base_branch: branch.clone(),
                }),
        );
        self.matches = matches;
        self.selected_index = 0;
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(choice) = self.matches.get(self.selected_index).cloned() {
            (self.on_confirm)(choice, window, cx);
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
        let (icon, label, detail) = match self.matches.get(ix)? {
            NewWorkspaceChoice::Local => (
                IconName::Folder,
                SharedString::from("Local checkout"),
                Some(SharedString::from("Work in the project's own folder")),
            ),
            NewWorkspaceChoice::Worktree { base_branch } => (
                IconName::GitBranch,
                SharedString::from(format!("New worktree from {base_branch}")),
                None,
            ),
        };
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                .child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(label))
                        .children(detail.map(|detail| {
                            Label::new(detail)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                        })),
                ),
        )
    }
}
