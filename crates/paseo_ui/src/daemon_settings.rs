use editor::Editor;
use gpui::{
    App, AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, PromptLevel,
    Subscription,
};
use paseo_client::Endpoint;
use remote::SshConnectionOptions;
use settings::{PaseoDaemonContent, SettingsContent, SettingsStore};
use ui::{Divider, Indicator, Label, LabelSize, Tooltip, prelude::*};
use workspace::{ModalView, Workspace};

use crate::{ConnectionStatus, PaseoDaemons, PaseoStore, daemon_auth, store::ENDPOINT_ENV_VAR};

gpui::actions!(
    paseo,
    [
        /// Lists the Paseo daemons in settings.json, to add, edit or remove them.
        ManageDaemons
    ]
);

pub(crate) fn open_daemons_dialog(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.toggle_modal(window, cx, DaemonsDialog::new);
}

/// Where settings.json has a daemon: `paseo.endpoint` (with `name` and `ssh`) at the top level,
/// or an entry of `paseo.daemons`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DaemonLocation {
    TopLevel,
    Listed(usize),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct DaemonFields {
    pub(crate) name: Option<String>,
    pub(crate) endpoint: Option<String>,
    pub(crate) ssh: Option<String>,
}

/// The daemons as written in the user's settings.json, in the sidebar's order.
pub(crate) fn configured_daemons(content: &SettingsContent) -> Vec<(DaemonLocation, DaemonFields)> {
    let Some(paseo) = content.paseo.as_ref() else {
        return Vec::new();
    };
    let mut daemons = Vec::new();
    let top_level = DaemonFields {
        name: paseo.name.clone(),
        endpoint: paseo.endpoint.clone(),
        ssh: paseo.ssh.clone(),
    };
    if top_level != DaemonFields::default() {
        daemons.push((DaemonLocation::TopLevel, top_level));
    }
    for (index, daemon) in paseo.daemons.iter().flatten().enumerate() {
        daemons.push((
            DaemonLocation::Listed(index),
            DaemonFields {
                name: daemon.name.clone(),
                endpoint: daemon.endpoint.clone(),
                ssh: daemon.ssh.clone(),
            },
        ));
    }
    daemons
}

/// Writes a daemon where settings.json has it (a new one, without a location, goes to the end
/// of `paseo.daemons`), or removes it when `fields` is `None`.
pub(crate) fn write_daemon(
    content: &mut SettingsContent,
    location: Option<DaemonLocation>,
    fields: Option<DaemonFields>,
) {
    let paseo = content.paseo.get_or_insert_default();
    match (location, fields) {
        (Some(DaemonLocation::TopLevel), fields) => {
            let fields = fields.unwrap_or_default();
            paseo.name = fields.name;
            paseo.endpoint = fields.endpoint;
            paseo.ssh = fields.ssh;
        }
        (Some(DaemonLocation::Listed(index)), fields) => {
            let Some(daemons) = paseo.daemons.as_mut() else {
                return;
            };
            if index >= daemons.len() {
                return;
            }
            match fields {
                Some(fields) => daemons[index] = daemon_content(fields),
                None => {
                    daemons.remove(index);
                }
            }
        }
        (None, Some(fields)) => paseo
            .daemons
            .get_or_insert_default()
            .push(daemon_content(fields)),
        (None, None) => {}
    }
    if paseo.daemons.as_ref().is_some_and(Vec::is_empty) {
        paseo.daemons = None;
    }
}

fn daemon_content(fields: DaemonFields) -> PaseoDaemonContent {
    PaseoDaemonContent {
        name: fields.name,
        endpoint: fields.endpoint,
        ssh: fields.ssh,
    }
}

/// Checks a daemon before it is saved; returns what is wrong.
pub(crate) fn validate(
    fields: &DaemonFields,
    location: Option<DaemonLocation>,
    others: &[(DaemonLocation, DaemonFields)],
) -> Result<(), String> {
    let Some(endpoint) = fields.endpoint.as_deref() else {
        return Err("Enter the daemon's endpoint, for example 100.64.0.2:6767.".into());
    };
    if let Err(error) = Endpoint::parse(endpoint) {
        return Err(format!("Invalid endpoint: {error:#}"));
    }
    let taken = others.iter().any(|(other_location, other)| {
        Some(*other_location) != location && other.endpoint.as_deref() == Some(endpoint)
    });
    if taken {
        return Err("Another daemon already has this endpoint.".into());
    }
    if let Some(ssh) = fields.ssh.as_deref()
        && let Err(error) = SshConnectionOptions::parse_command_line(ssh)
    {
        return Err(format!("Invalid SSH host: {error:#}"));
    }
    Ok(())
}

fn non_empty(text: String) -> Option<String> {
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

struct DaemonForm {
    location: Option<DaemonLocation>,
    name: Entity<Editor>,
    endpoint: Entity<Editor>,
    ssh: Entity<Editor>,
    error: Option<SharedString>,
}

/// Lists the Paseo daemons of settings.json and adds, edits and removes them there, so they need
/// not be edited by hand.
pub(crate) struct DaemonsDialog {
    focus_handle: FocusHandle,
    form: Option<DaemonForm>,
    _settings_subscription: Subscription,
}

impl DaemonsDialog {
    fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            form: None,
            _settings_subscription: cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
        }
    }

    fn daemons(cx: &App) -> Vec<(DaemonLocation, DaemonFields)> {
        cx.global::<SettingsStore>()
            .raw_user_settings()
            .map(|settings| configured_daemons(&settings.content))
            .unwrap_or_default()
    }

    fn start_editing(
        &mut self,
        location: Option<DaemonLocation>,
        fields: DaemonFields,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = |text: Option<String>,
                      placeholder: &str,
                      window: &mut Window,
                      cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor.set_text(text.unwrap_or_default(), window, cx);
                editor
            })
        };
        let name = editor(fields.name, "The daemon's host name", window, cx);
        let endpoint = editor(fields.endpoint, "100.64.0.2:6767", window, cx);
        let ssh = editor(
            fields.ssh,
            "None: the daemon is on this machine",
            window,
            cx,
        );
        endpoint.focus_handle(cx).focus(window, cx);
        self.form = Some(DaemonForm {
            location,
            name,
            endpoint,
            ssh,
            error: None,
        });
        cx.notify();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let fields = DaemonFields {
            name: non_empty(form.name.read(cx).text(cx)),
            endpoint: non_empty(form.endpoint.read(cx).text(cx)),
            ssh: non_empty(form.ssh.read(cx).text(cx)),
        };
        let location = form.location;
        if let Err(error) = validate(&fields, location, &Self::daemons(cx)) {
            form.error = Some(error.into());
            cx.notify();
            return;
        }
        settings::update_settings_file(<dyn fs::Fs>::global(cx), cx, move |content, _| {
            write_daemon(content, location, Some(fields));
        });
        self.form = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn remove(
        &mut self,
        location: DaemonLocation,
        fields: DaemonFields,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = fields
            .name
            .clone()
            .or_else(|| fields.endpoint.clone())
            .unwrap_or_else(|| "this daemon".into());
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Remove {label} from Pazedo?"),
            Some(
                "Its workspaces leave the sidebar. The daemon, its agents and its workspaces \
                 keep running.",
            ),
            &["Remove", "Cancel"],
            cx,
        );
        cx.spawn(async move |_, cx| {
            if answer.await != Ok(0) {
                return;
            }
            cx.update(|cx| {
                settings::update_settings_file(<dyn fs::Fs>::global(cx), cx, move |content, _| {
                    write_daemon(content, Some(location), None);
                });
                if let Some(endpoint) = fields.endpoint {
                    daemon_auth::forget_password(daemon_auth::password_url(&endpoint), cx).detach();
                }
            });
        })
        .detach();
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.save(window, cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.form.take().is_some() {
            self.focus_handle.focus(window, cx);
            cx.notify();
        } else {
            cx.emit(DismissEvent);
        }
    }

    /// Tab moves between the form's fields.
    fn focus_next_field(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.form else {
            return;
        };
        // In the order they are shown.
        let fields = [&form.endpoint, &form.name, &form.ssh];
        let current = fields
            .iter()
            .position(|field| field.focus_handle(cx).is_focused(window))
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % fields.len()
        } else {
            (current + fields.len() - 1) % fields.len()
        };
        if let Some(field) = fields.get(next) {
            field.focus_handle(cx).focus(window, cx);
        }
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let daemons = Self::daemons(cx);
        let stores = PaseoDaemons::global(cx).read(cx).stores().to_vec();
        let rows = daemons
            .into_iter()
            .enumerate()
            .map(|(index, (location, fields))| {
                let store = stores
                    .iter()
                    .find(|store| store.read(cx).endpoint_setting() == fields.endpoint.as_deref());
                self.render_daemon(index, location, fields, store, cx)
            });
        let override_notice = std::env::var(ENDPOINT_ENV_VAR)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|endpoint| {
                Label::new(format!(
                    "{ENDPOINT_ENV_VAR} is set ({endpoint}), so Pazedo connects only to that \
                     daemon until it is unset; these are the daemons of settings.json."
                ))
                .size(LabelSize::Small)
                .color(Color::Warning)
            });
        v_flex()
            .gap_1()
            .children(override_notice)
            .children(rows)
            .when(Self::daemons(cx).is_empty(), |this| {
                this.child(
                    Label::new("No daemons yet.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .child(Divider::horizontal())
            .child(
                h_flex().justify_end().child(
                    Button::new("paseo-add-daemon", "Add Daemon")
                        .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.start_editing(None, DaemonFields::default(), window, cx)
                        })),
                ),
            )
            .into_any_element()
    }

    fn render_daemon(
        &self,
        index: usize,
        location: DaemonLocation,
        fields: DaemonFields,
        store: Option<&Entity<PaseoStore>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (status_color, status) = match store.map(|store| store.read(cx).status()) {
            Some(ConnectionStatus::Connected(_)) => (Color::Success, "Connected"),
            Some(ConnectionStatus::Connecting(_)) => (Color::Warning, "Connecting"),
            Some(ConnectionStatus::PasswordRequired { .. }) => {
                (Color::Warning, "Password required")
            }
            Some(ConnectionStatus::Disconnected(_)) => (Color::Error, "Disconnected"),
            Some(ConnectionStatus::InvalidEndpoint(_)) => (Color::Error, "Invalid endpoint"),
            Some(ConnectionStatus::NotConfigured) | None => (Color::Muted, "Not connected"),
        };
        let name: SharedString = fields
            .name
            .clone()
            .map(Into::into)
            .or_else(|| store.map(|store| store.read(cx).display_name()))
            .unwrap_or_else(|| "Unnamed daemon".into());
        let mut details = fields.endpoint.clone().unwrap_or_default();
        if let Some(ssh) = &fields.ssh {
            details.push_str(&format!(" · SSH {ssh}"));
        }
        h_flex()
            .id(("paseo-daemon-entry", index))
            .w_full()
            .px_1()
            .py_1()
            .gap_2()
            .rounded_sm()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .child(
                div()
                    .id(("paseo-daemon-status", index))
                    .child(Indicator::dot().color(status_color))
                    .tooltip(Tooltip::text(status)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(name).truncate())
                    .child(
                        Label::new(details)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .child(
                IconButton::new(("paseo-edit-daemon", index), IconName::Pencil)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Edit Daemon"))
                    .on_click(cx.listener({
                        let fields = fields.clone();
                        move |this, _, window, cx| {
                            this.start_editing(Some(location), fields.clone(), window, cx)
                        }
                    })),
            )
            .child(
                IconButton::new(("paseo-remove-daemon", index), IconName::Trash)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Remove Daemon…"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.remove(location, fields.clone(), window, cx)
                    })),
            )
            .into_any_element()
    }

    fn render_form(&self, form: &DaemonForm, cx: &mut Context<Self>) -> AnyElement {
        let field = |label: &'static str, hint: &'static str, editor: &Entity<Editor>| {
            v_flex()
                .gap_0p5()
                .child(Label::new(label).size(LabelSize::Small))
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .border_1()
                        .border_color(cx.theme().colors().border)
                        .rounded_sm()
                        .child(editor.clone()),
                )
                .child(Label::new(hint).size(LabelSize::XSmall).color(Color::Muted))
        };
        v_flex()
            .gap_2()
            .capture_action(cx.listener(|this, _: &editor::actions::Tab, window, cx| {
                this.focus_next_field(true, window, cx);
            }))
            .capture_action(
                cx.listener(|this, _: &editor::actions::Backtab, window, cx| {
                    this.focus_next_field(false, window, cx);
                }),
            )
            .child(field(
                "Endpoint",
                "host:port, ws://host:port/ws or a Unix socket path.",
                &form.endpoint,
            ))
            .child(field(
                "Name",
                "Shown in the sidebar. Empty: the daemon's host name.",
                &form.name,
            ))
            .child(field(
                "SSH host",
                "For a daemon on another machine, e.g. root@devbox; its folders open over SSH.",
                &form.ssh,
            ))
            .children(
                form.error
                    .clone()
                    .map(|error| Label::new(error).size(LabelSize::Small).color(Color::Error)),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(Button::new("paseo-daemon-cancel", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                    ))
                    .child(
                        Button::new("paseo-daemon-save", "Save")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    ),
            )
            .into_any_element()
    }
}

impl EventEmitter<DismissEvent> for DaemonsDialog {}
impl ModalView for DaemonsDialog {}

impl Focusable for DaemonsDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DaemonsDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match &self.form {
            None => "Paseo Daemons",
            Some(form) if form.location.is_none() => "Add Daemon",
            Some(_) => "Edit Daemon",
        };
        let body = match &self.form {
            Some(form) => self.render_form(form, cx),
            None => self.render_list(cx),
        };
        v_flex()
            .key_context("PaseoDaemonsDialog")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(rems(34.))
            .p_3()
            .gap_2()
            .child(Label::new(title))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use settings::PaseoSettingsContent;

    fn fields(name: Option<&str>, endpoint: &str, ssh: Option<&str>) -> DaemonFields {
        DaemonFields {
            name: name.map(str::to_string),
            endpoint: Some(endpoint.to_string()),
            ssh: ssh.map(str::to_string),
        }
    }

    fn settings() -> SettingsContent {
        SettingsContent {
            paseo: Some(PaseoSettingsContent {
                endpoint: Some("100.64.0.1:6767".into()),
                daemons: Some(vec![PaseoDaemonContent {
                    name: Some("dev box".into()),
                    endpoint: Some("100.64.0.2:6767".into()),
                    ssh: Some("root@devbox".into()),
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn lists_the_top_level_daemon_then_the_listed_ones() {
        assert_eq!(
            configured_daemons(&settings()),
            vec![
                (
                    DaemonLocation::TopLevel,
                    fields(None, "100.64.0.1:6767", None)
                ),
                (
                    DaemonLocation::Listed(0),
                    fields(Some("dev box"), "100.64.0.2:6767", Some("root@devbox"))
                ),
            ]
        );
    }

    #[test]
    fn edits_adds_and_removes_daemons_where_settings_have_them() {
        let mut content = settings();
        write_daemon(
            &mut content,
            Some(DaemonLocation::TopLevel),
            Some(fields(Some("mini"), "100.64.0.1:6767", None)),
        );
        write_daemon(
            &mut content,
            None,
            Some(fields(None, "127.0.0.1:6868", None)),
        );
        write_daemon(&mut content, Some(DaemonLocation::Listed(0)), None);
        assert_eq!(
            configured_daemons(&content),
            vec![
                (
                    DaemonLocation::TopLevel,
                    fields(Some("mini"), "100.64.0.1:6767", None)
                ),
                (
                    DaemonLocation::Listed(0),
                    fields(None, "127.0.0.1:6868", None)
                ),
            ]
        );

        write_daemon(&mut content, Some(DaemonLocation::Listed(0)), None);
        write_daemon(&mut content, Some(DaemonLocation::TopLevel), None);
        assert_eq!(configured_daemons(&content), Vec::new());
        assert_eq!(
            content.paseo.and_then(|paseo| paseo.daemons),
            None,
            "an emptied list is removed rather than left as []"
        );
    }

    #[test]
    fn validates_endpoint_uniqueness_and_ssh() {
        let others = configured_daemons(&settings());
        assert!(validate(&DaemonFields::default(), None, &others).is_err());
        assert!(
            validate(&fields(None, "100.64.0.2:6767", None), None, &others).is_err(),
            "the dev box's endpoint is taken"
        );
        assert!(
            validate(
                &fields(Some("dev"), "100.64.0.2:6767", Some("root@devbox")),
                Some(DaemonLocation::Listed(0)),
                &others
            )
            .is_ok(),
            "editing a daemon keeps its own endpoint"
        );
        assert!(validate(&fields(None, "127.0.0.1:6868", None), None, &others).is_ok());
    }
}
