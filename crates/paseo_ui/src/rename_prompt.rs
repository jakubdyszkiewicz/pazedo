use editor::Editor;
use gpui::{
    App, AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, WeakEntity,
};
use ui::{Label, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, MultiWorkspace, Workspace};

type OnRename = Box<dyn FnOnce(String, &mut Window, &mut App)>;

/// Asks for a new name for an agent or terminal tab, starting from the current one.
pub(crate) struct RenamePrompt {
    editor: Entity<Editor>,
    heading: SharedString,
    current_name: String,
    on_rename: Option<OnRename>,
}

impl RenamePrompt {
    /// Opens the prompt in `workspace`. `on_rename` gets the new name, trimmed; an empty or
    /// unchanged one closes the prompt without it.
    pub(crate) fn open(
        workspace: &WeakEntity<Workspace>,
        heading: impl Into<SharedString>,
        current_name: String,
        on_rename: impl FnOnce(String, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut App,
    ) {
        let heading = heading.into();
        workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    let editor = cx.new(|cx| {
                        let mut editor = Editor::single_line(window, cx);
                        editor.set_text(current_name.clone(), window, cx);
                        editor.select_all(&editor::actions::SelectAll, window, cx);
                        editor
                    });
                    Self {
                        editor,
                        heading,
                        current_name,
                        on_rename: Some(Box::new(on_rename)),
                    }
                });
            })
            .ok();
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.editor.read(cx).text(cx).trim().to_string();
        if !name.is_empty()
            && name != self.current_name
            && let Some(on_rename) = self.on_rename.take()
        {
            on_rename(name, window, cx);
        }
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for RenamePrompt {}
impl ModalView for RenamePrompt {}

impl Focusable for RenamePrompt {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for RenamePrompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PaseoRenamePrompt")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w_96()
            .overflow_hidden()
            .child(div().px_2().pt_2().child(Label::new(self.heading.clone())))
            .child(div().p_2().child(self.editor.clone()))
    }
}

/// Runs `f` with the active window's workspace, for actions handled app-wide.
pub(crate) fn with_active_workspace(
    cx: &mut App,
    f: impl FnOnce(&Entity<Workspace>, &mut Window, &mut App),
) {
    let Some(window) = cx.active_window() else {
        return;
    };
    window
        .update(cx, |_, window, cx| {
            let Some(multi_workspace) = window.root::<MultiWorkspace>().flatten() else {
                return;
            };
            let workspace = multi_workspace.read(cx).workspace().clone();
            f(&workspace, window, cx);
        })
        .log_err();
}
