use collections::HashSet;
use gpui::{App, Context, DismissEvent, Entity, SharedString, Task, Window};
use paseo_client::protocol::{CloneProtocol, GithubRepository};
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

/// A repository to clone: one from the daemon's GitHub search, or one typed as a URL or
/// `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CloneSource {
    pub name_with_owner: String,
    /// What the daemon clones: a clone URL, or `owner/name` with `clone_protocol`.
    pub repo: String,
    pub clone_protocol: Option<CloneProtocol>,
    pub description: Option<String>,
    pub is_private: bool,
}

impl CloneSource {
    /// The folder the clone gets: the repository's name.
    pub fn folder_name(&self) -> &str {
        let name = self
            .name_with_owner
            .rsplit('/')
            .next()
            .unwrap_or(&self.name_with_owner);
        name.strip_suffix(".git").unwrap_or(name)
    }

    fn from_repository(repository: GithubRepository) -> Self {
        Self {
            is_private: repository.visibility != "public",
            name_with_owner: repository.name_with_owner,
            repo: repository.clone_url,
            clone_protocol: None,
            description: repository.description,
        }
    }
}

/// The repositories a typed query names by itself: a clone URL, or `owner/name` over HTTPS and
/// over SSH, as in Paseo's app.
pub(crate) fn typed_clone_sources(query: &str) -> Vec<CloneSource> {
    let query = query.trim();
    let is_remote = ["https://", "http://", "ssh://", "git@"]
        .iter()
        .any(|prefix| query.starts_with(prefix));
    if is_remote {
        let path = query
            .split_once("github.com")
            .map(|(_, path)| path.trim_start_matches([':', '/']))
            .unwrap_or_else(|| query.rsplit(['/', ':']).next().unwrap_or(query));
        let path = path.trim_end_matches('/');
        return vec![CloneSource {
            name_with_owner: path.strip_suffix(".git").unwrap_or(path).to_string(),
            repo: query.to_string(),
            clone_protocol: None,
            description: Some("Clone this URL".into()),
            is_private: false,
        }];
    }
    let Some((owner, name)) = query.split_once('/') else {
        return Vec::new();
    };
    let is_part =
        |part: &str| !part.is_empty() && !part.contains('/') && !part.contains(char::is_whitespace);
    if !is_part(owner) || !is_part(name) {
        return Vec::new();
    }
    [(CloneProtocol::Https, "HTTPS"), (CloneProtocol::Ssh, "SSH")]
        .into_iter()
        .map(|(protocol, label)| CloneSource {
            name_with_owner: query.to_string(),
            repo: query.to_string(),
            clone_protocol: Some(protocol),
            description: Some(format!("Clone {query} over {label}")),
            is_private: false,
        })
        .collect()
}

enum CloneStage {
    Repository,
    Location(CloneSource),
}

enum CloneMatch {
    Repository(CloneSource),
    Location(String),
}

type OnClone = Rc<dyn Fn(CloneSource, String, &mut Window, &mut App)>;

/// Clones a GitHub repository on the daemon's machine in two steps: the repository, searched
/// with the daemon's `gh` or typed, then the folder to clone it into.
pub(crate) struct CloneProjectDelegate {
    store: Entity<PaseoStore>,
    host_name: SharedString,
    stage: CloneStage,
    matches: Vec<CloneMatch>,
    selected_index: usize,
    error: Option<SharedString>,
    parent_suggestions: Vec<String>,
    on_clone: OnClone,
}

/// Opens the clone picker in `workspace`; `on_clone` gets the repository and the folder to
/// clone it into.
pub(crate) fn open_clone_project_picker(
    workspace: &mut Workspace,
    store: Entity<PaseoStore>,
    host_name: SharedString,
    on_clone: impl Fn(CloneSource, String, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let parent_suggestions = store.read(cx).clone_parent_suggestions();
    workspace.toggle_modal(window, cx, move |window, cx| {
        let delegate = CloneProjectDelegate {
            store,
            host_name,
            stage: CloneStage::Repository,
            matches: Vec::new(),
            selected_index: 0,
            error: None,
            parent_suggestions,
            on_clone: Rc::new(on_clone),
        };
        Picker::uniform_list(delegate, window, cx)
    });
}

impl CloneProjectDelegate {
    fn search_repositories(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        cx.spawn_in(window, async move |picker, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let Some(search) = picker
                .update(cx, |picker, cx| {
                    picker
                        .delegate
                        .store
                        .read(cx)
                        .search_github_repositories(query.trim().to_string(), cx)
                })
                .ok()
            else {
                return;
            };
            let result = search.await;
            picker
                .update(cx, |picker, cx| {
                    let delegate = &mut picker.delegate;
                    let mut sources = typed_clone_sources(&query);
                    let lowercase_query = query.trim().to_lowercase();
                    match result {
                        Ok(repositories) => {
                            let found = repositories.iter().any(|repository| {
                                repository.name_with_owner.to_lowercase() == lowercase_query
                                    || repository.clone_url.to_lowercase() == lowercase_query
                            });
                            if found {
                                sources.clear();
                            }
                            sources
                                .extend(repositories.into_iter().map(CloneSource::from_repository));
                            delegate.error = None;
                        }
                        Err(error) => delegate.error = Some(format!("{error:#}").into()),
                    }
                    delegate.matches = sources.into_iter().map(CloneMatch::Repository).collect();
                    delegate.selected_index = 0;
                    cx.notify();
                })
                .ok();
        })
    }

    fn suggest_locations(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_string();
        let lowercase_query = query.to_lowercase();
        let suggested: Vec<String> = self
            .parent_suggestions
            .iter()
            .filter(|parent| parent.to_lowercase().contains(&lowercase_query))
            .cloned()
            .collect();
        if query.is_empty() {
            self.matches_from(suggested, cx);
            return Task::ready(());
        }
        cx.spawn_in(window, async move |picker, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            let Some(suggestions) = picker
                .update(cx, |picker, cx| {
                    picker
                        .delegate
                        .store
                        .read(cx)
                        .directory_suggestions(query.clone(), cx)
                })
                .ok()
            else {
                return;
            };
            let result = suggestions.await;
            picker
                .update(cx, |picker, cx| {
                    let mut folders = Vec::new();
                    // A typed path is offered as is: the daemon creates a missing folder.
                    let typed = query.trim_end_matches('/');
                    if typed.starts_with('/') || typed.starts_with('~') {
                        folders.push(typed.to_string());
                    }
                    folders.extend(suggested);
                    match result {
                        Ok(directories) => {
                            folders.extend(directories);
                            picker.delegate.error = None;
                        }
                        Err(error) => picker.delegate.error = Some(format!("{error:#}").into()),
                    }
                    let mut seen = HashSet::default();
                    folders.retain(|folder| seen.insert(folder.clone()));
                    picker.delegate.matches_from(folders, cx);
                })
                .ok();
        })
    }

    fn matches_from(&mut self, folders: Vec<String>, cx: &mut Context<Picker<Self>>) {
        self.matches = folders.into_iter().map(CloneMatch::Location).collect();
        self.selected_index = 0;
        cx.notify();
    }
}

impl PickerDelegate for CloneProjectDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "paseo-clone-project"
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
        match &self.stage {
            CloneStage::Repository => format!(
                "Search GitHub from {}, or enter a URL or owner/repo",
                self.host_name
            ),
            CloneStage::Location(source) => format!(
                "Folder on {} to clone {} into, e.g. ~/dev",
                self.host_name,
                source.folder_name()
            ),
        }
        .into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(self.error.clone().unwrap_or_else(|| match self.stage {
            CloneStage::Repository => "No matching repositories".into(),
            CloneStage::Location(_) => "Type a folder, e.g. ~/dev".into(),
        }))
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        match self.stage {
            CloneStage::Repository => self.search_repositories(query, window, cx),
            CloneStage::Location(_) => self.suggest_locations(query, window, cx),
        }
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        match self.matches.get(self.selected_index) {
            Some(CloneMatch::Repository(source)) => {
                self.stage = CloneStage::Location(source.clone());
                self.matches.clear();
                self.error = None;
                cx.defer_in(window, |picker, window, cx| {
                    picker.set_query("", window, cx);
                    picker.refresh_placeholder(window, cx);
                    picker.refresh(window, cx);
                });
            }
            Some(CloneMatch::Location(parent)) => {
                if let CloneStage::Location(source) = &self.stage {
                    (self.on_clone)(source.clone(), parent.clone(), window, cx);
                }
                cx.emit(DismissEvent);
            }
            None => {}
        }
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
        let item = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected);
        Some(match self.matches.get(ix)? {
            CloneMatch::Repository(source) => item
                .start_slot(
                    Icon::new(IconName::Github)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .min_w_0()
                        .child(Label::new(source.name_with_owner.clone()))
                        .when(source.is_private, |this| {
                            this.child(
                                Label::new("private")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        })
                        .when_some(source.description.clone(), |this, description| {
                            this.child(
                                Label::new(description)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                        }),
                ),
            CloneMatch::Location(parent) => {
                let CloneStage::Location(source) = &self.stage else {
                    return None;
                };
                let checkout = format!("{}/{}", parent.trim_end_matches('/'), source.folder_name());
                item.start_slot(
                    Icon::new(IconName::Folder)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(Label::new(checkout))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_repositories() {
        let names = |query: &str| -> Vec<(String, Option<CloneProtocol>, String)> {
            typed_clone_sources(query)
                .into_iter()
                .map(|source| {
                    let folder = source.folder_name().to_string();
                    (source.name_with_owner, source.clone_protocol, folder)
                })
                .collect()
        };
        assert_eq!(
            names(" getpaseo/paseo "),
            vec![
                (
                    "getpaseo/paseo".into(),
                    Some(CloneProtocol::Https),
                    "paseo".into()
                ),
                (
                    "getpaseo/paseo".into(),
                    Some(CloneProtocol::Ssh),
                    "paseo".into()
                ),
            ]
        );
        assert_eq!(
            names("https://github.com/zed-industries/zed.git"),
            vec![("zed-industries/zed".into(), None, "zed".into())]
        );
        assert_eq!(
            names("git@github.com:octocat/Hello-World.git"),
            vec![("octocat/Hello-World".into(), None, "Hello-World".into())]
        );
        assert_eq!(
            names("https://gitlab.com/group/tool.git"),
            vec![("tool".into(), None, "tool".into())]
        );
        assert!(names("paseo").is_empty());
        assert!(names("a/b/c").is_empty());
        assert!(names("my repo/x").is_empty());
    }
}
