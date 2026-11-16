//! Pazedo: type-to-find. Typing in the focused panel selects the visible entry whose name
//! matches best, without filtering the tree (see the `type_ahead` crate). While a search is on,
//! the panel's key context has `type_ahead`, for the keymap to give backspace and space to the
//! search and enter to opening the match; escape and moving the selection end it.

use gpui::{AnyElement, App, Context, KeyDownEvent, Window};
use type_ahead::TypeAhead;
use workspace::SelectedEntry;

use crate::ProjectPanel;

#[derive(Default)]
pub(crate) struct TypeToFind {
    type_ahead: TypeAhead,
    /// The selection the search left, to tell when something else moved it.
    selection: Option<SelectedEntry>,
}

impl TypeToFind {
    pub(crate) fn is_active(&self) -> bool {
        self.type_ahead.is_active()
    }

    pub(crate) fn end(&mut self) -> bool {
        self.type_ahead.end()
    }
}

impl ProjectPanel {
    /// The visible entries in order, with the name each row shows: a worktree's root name for
    /// its root, else the file name.
    fn type_to_find_candidates(&self, cx: &App) -> Vec<(SelectedEntry, String)> {
        let project = self.project.read(cx);
        let mut candidates = Vec::new();
        for worktree_entries in &self.state.visible_entries {
            let Some(worktree) = project.worktree_for_id(worktree_entries.worktree_id, cx) else {
                continue;
            };
            let root_name = worktree.read(cx).root_name_str();
            for entry in &worktree_entries.entries {
                let name = entry.path.file_name().unwrap_or(root_name);
                candidates.push((
                    SelectedEntry {
                        worktree_id: worktree_entries.worktree_id,
                        entry_id: entry.id,
                    },
                    name.to_string(),
                ));
            }
        }
        candidates
    }

    pub(crate) fn type_to_find_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only the tree itself: not the file name editor or an open menu.
        if !self.focus_handle.is_focused(window) || self.state.edit_state.is_some() {
            return;
        }
        if !self.type_to_find.type_ahead.handle_key(&event.keystroke) {
            return;
        }
        cx.stop_propagation();
        let candidates = self.type_to_find_candidates(cx);
        let best = self
            .type_to_find
            .type_ahead
            .best_match(candidates.iter().map(|(_, name)| Some(name.as_str())));
        if let Some((selection, _)) = best.and_then(|position| candidates.get(position)) {
            self.selection = Some(*selection);
            self.autoscroll(cx);
        }
        self.type_to_find.selection = self.selection;
        cx.notify();
    }

    /// Ends the search once the selection moved otherwise (arrows, a click) or the panel lost
    /// the focus.
    pub(crate) fn end_moved_type_to_find(&mut self, window: &Window) {
        if self.type_to_find.is_active()
            && (self.selection != self.type_to_find.selection
                || !self.focus_handle.is_focused(window))
        {
            self.type_to_find.end();
        }
    }

    pub(crate) fn render_type_to_find(&self, cx: &App) -> Option<AnyElement> {
        if !self.type_to_find.is_active() {
            return None;
        }
        let candidates = self.type_to_find_candidates(cx);
        let has_match = self
            .type_to_find
            .type_ahead
            .best_match(candidates.iter().map(|(_, name)| Some(name.as_str())))
            .is_some();
        self.type_to_find
            .type_ahead
            .render(has_match, "Enter to open", cx)
    }
}

#[cfg(test)]
mod tests {
    use gpui::{BorrowAppContext as _, Entity, TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use workspace::{AppState, MultiWorkspace};

    use crate::{OpenPermanent, ProjectPanel, SelectNext};

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let app_state = AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
            workspace::init(app_state, cx);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.worktree.file_scan_depth = Some(0);
                });
            });
        });
    }

    fn selected_path(panel: &Entity<ProjectPanel>, cx: &mut VisualTestContext) -> Option<String> {
        panel.read_with(cx, |panel, cx| {
            let (_, entry) = panel.selected_entry(cx)?;
            Some(entry.path.as_unix_str().to_string())
        })
    }

    fn query(panel: &Entity<ProjectPanel>, cx: &mut VisualTestContext) -> Option<String> {
        panel.read_with(cx, |panel, _| {
            panel.type_to_find.type_ahead.query().map(str::to_string)
        })
    }

    #[gpui::test]
    async fn typing_selects_the_best_matching_entry(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project_root",
            json!({
                "src": { "main.rs": "" },
                "Cargo.toml": "",
                "README.md": "",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), ["/project_root".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = ProjectPanel::new(workspace, window, cx);
            workspace.add_panel(panel.clone(), window, cx);
            workspace.focus_panel::<ProjectPanel>(window, cx);
            panel
        });
        cx.run_until_parked();

        cx.simulate_keystrokes("r e a");
        assert_eq!(selected_path(&panel, cx).as_deref(), Some("README.md"));
        assert_eq!(query(&panel, cx).as_deref(), Some("rea"));

        cx.simulate_keystrokes("backspace backspace backspace s r c");
        assert_eq!(selected_path(&panel, cx).as_deref(), Some("src"));

        cx.simulate_keystrokes("x");
        assert_eq!(
            selected_path(&panel, cx).as_deref(),
            Some("src"),
            "no match keeps the selection"
        );
        assert_eq!(query(&panel, cx).as_deref(), Some("srcx"));

        cx.dispatch_action(menu::Cancel);
        assert_eq!(query(&panel, cx), None, "escape ends the search");
        assert_eq!(selected_path(&panel, cx).as_deref(), Some("src"));

        cx.simulate_keystrokes("c a r");
        assert_eq!(selected_path(&panel, cx).as_deref(), Some("Cargo.toml"));
        cx.dispatch_action(SelectNext);
        cx.run_until_parked();
        assert_eq!(selected_path(&panel, cx).as_deref(), Some("README.md"));
        assert_eq!(
            query(&panel, cx),
            None,
            "moving the selection ends the search"
        );

        cx.simulate_keystrokes("c a r");
        cx.dispatch_action(OpenPermanent);
        cx.run_until_parked();
        assert_eq!(query(&panel, cx), None, "opening the match ends the search");
        let opened = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.project_path(cx))
                .map(|path| path.path.as_unix_str().to_string())
        });
        assert_eq!(opened.as_deref(), Some("Cargo.toml"));
    }
}
