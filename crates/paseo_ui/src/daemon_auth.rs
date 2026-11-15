use std::{net::IpAddr, path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use credentials_provider::CredentialsProvider;
use editor::Editor;
use gpui::{
    App, AppContext as _, AsyncApp, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Task,
};
use paseo_client::{Endpoint, protocol::HelloAuth};
use ui::{Label, LabelSize, prelude::*};
use util::ResultExt as _;
use workspace::ModalView;

use crate::PaseoStore;

/// The account name of saved daemon passwords; the keychain item is keyed by URL.
const PASSWORD_USERNAME: &str = "paseo";

/// Where a daemon's password is saved: per endpoint, as configured.
pub(crate) fn password_url(endpoint: &str) -> String {
    format!("paseo://{}", endpoint.trim())
}

/// Release builds keep passwords in the macOS Keychain. Debug builds use Zed's development
/// credentials file instead (in the build's config directory), as Zed's own debug builds do:
/// each rebuild is signed differently, so the Keychain would ask for access after every build.
/// Pazedo.app is on Zed's dev release channel, where `zed_credentials_provider` would pick that
/// file too, hence the explicit choice here.
fn credentials_provider(cx: &App) -> Arc<dyn CredentialsProvider> {
    if cfg!(debug_assertions) {
        zed_credentials_provider::global(cx)
    } else {
        Arc::new(KeychainCredentials)
    }
}

struct KeychainCredentials;

impl CredentialsProvider for KeychainCredentials {
    fn read_credentials<'a>(
        &'a self,
        url: &'a str,
        cx: &'a AsyncApp,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>> {
        Box::pin(async move { cx.update(|cx| cx.read_credentials(url)).await })
    }

    fn write_credentials<'a>(
        &'a self,
        url: &'a str,
        username: &'a str,
        password: &'a [u8],
        cx: &'a AsyncApp,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        Box::pin(async move {
            cx.update(move |cx| cx.write_credentials(url, username, password))
                .await
        })
    }

    fn delete_credentials<'a>(
        &'a self,
        url: &'a str,
        cx: &'a AsyncApp,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        Box::pin(async move { cx.update(|cx| cx.delete_credentials(url)).await })
    }
}

/// What to send in the first `hello`: the password saved for the daemon, else the local
/// credential of a daemon on this machine, else nothing (daemons without a password).
pub(crate) async fn initial_auth(
    endpoint: Endpoint,
    url: String,
    cx: &mut AsyncApp,
) -> Option<HelloAuth> {
    let provider = cx.update(|cx| credentials_provider(cx));
    match provider.read_credentials(&url, cx).await {
        Ok(Some((_, password))) => match String::from_utf8(password) {
            Ok(password) => return Some(HelloAuth::Password { password }),
            Err(error) => log::error!("the saved password for {url} is not UTF-8: {error}"),
        },
        Ok(None) => {}
        Err(error) => log::error!("failed to read the saved password for {url}: {error:#}"),
    }
    cx.background_spawn(async move { local_credential(&endpoint) })
        .await
        .map(|token| HelloAuth::LocalCredential { token })
}

pub(crate) fn save_password(url: String, password: String, cx: &mut App) -> Task<()> {
    let provider = credentials_provider(cx);
    cx.spawn(async move |cx| {
        provider
            .write_credentials(&url, PASSWORD_USERNAME, password.as_bytes(), cx)
            .await
            .with_context(|| format!("failed to save the password for {url}"))
            .log_err();
    })
}

/// Deletes the saved password of a daemon that was removed.
pub(crate) fn forget_password(url: String, cx: &mut App) -> Task<()> {
    let provider = credentials_provider(cx);
    cx.spawn(async move |cx| {
        provider
            .delete_credentials(&url, cx)
            .await
            .with_context(|| format!("failed to delete the password for {url}"))
            .log_err();
    })
}

/// The token a daemon on this machine writes for local clients (`$PASEO_HOME/local-credential`,
/// readable only by its user). It admits like the password, so local daemons need no password.
fn local_credential(endpoint: &Endpoint) -> Option<String> {
    // Tests connect to 127.0.0.1 too, and must not read the user's credential.
    if cfg!(test) || !is_on_this_machine(endpoint) {
        return None;
    }
    let home = std::env::var_os("PASEO_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home_dir().join(".paseo"));
    let token = std::fs::read_to_string(home.join("local-credential")).ok()?;
    let token = token.trim();
    // The daemon only accepts its own format: 32 random bytes in unpadded base64url.
    let is_token = token.len() == 43
        && token
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character));
    is_token.then(|| token.to_string())
}

/// Whether the endpoint is an address of this machine: a Unix socket, `localhost`, a loopback
/// address, or an address one of this machine's interfaces has (binding to it only succeeds
/// then), such as its own Tailscale address. Host names other than `localhost` are not
/// resolved, so their daemons count as remote.
fn is_on_this_machine(endpoint: &Endpoint) -> bool {
    match endpoint {
        Endpoint::Unix(_) => true,
        Endpoint::Tcp { host, .. } => {
            if host.eq_ignore_ascii_case("localhost") {
                return true;
            }
            let host = host.trim_start_matches('[').trim_end_matches(']');
            match host.parse::<IpAddr>() {
                Ok(address) => {
                    address.is_loopback() || std::net::UdpSocket::bind((address, 0)).is_ok()
                }
                Err(_) => false,
            }
        }
    }
}

/// Asks for a daemon's password. The daemon checks it; it is saved once the daemon accepts it.
pub(crate) struct PasswordPrompt {
    store: Entity<PaseoStore>,
    editor: Entity<Editor>,
    daemon_name: SharedString,
    rejected: bool,
}

impl PasswordPrompt {
    pub(crate) fn new(
        store: Entity<PaseoStore>,
        rejected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_masked(true, cx);
            editor.set_placeholder_text("Password", window, cx);
            editor
        });
        let daemon_name = store.read(cx).display_name();
        Self {
            store,
            editor,
            daemon_name,
            rejected,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let password = self.editor.read(cx).text(cx);
        if password.is_empty() {
            return;
        }
        self.store
            .update(cx, |store, cx| store.submit_password(password, cx));
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for PasswordPrompt {}
impl ModalView for PasswordPrompt {}

impl Focusable for PasswordPrompt {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for PasswordPrompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let (hint, hint_color) = if self.rejected {
            ("The daemon rejected the password.", Color::Error)
        } else if cfg!(debug_assertions) {
            (
                "Saved for this build once the daemon accepts it.",
                Color::Muted,
            )
        } else {
            (
                "Saved in the Keychain once the daemon accepts it.",
                Color::Muted,
            )
        };
        v_flex()
            .key_context("PaseoPasswordPrompt")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w_96()
            .overflow_hidden()
            .child(
                div()
                    .px_2()
                    .pt_2()
                    .child(Label::new(format!("Password for {}", self.daemon_name))),
            )
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(self.editor.clone()),
            )
            .child(
                h_flex()
                    .bg(colors.editor_background)
                    .w_full()
                    .p_2()
                    .child(Label::new(hint).size(LabelSize::Small).color(hint_color)),
            )
    }
}
