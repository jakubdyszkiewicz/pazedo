# pazedo: a Zed fork that is a Paseo client

pazedo is a fork of [Zed](https://github.com/zed-industries/zed) that uses a
[Paseo](https://paseo.sh) daemon for workspaces, agent sessions and terminals, while Zed keeps
doing editing, files, language servers, git and search. The full brief (goal, research,
milestones) lives outside the repo in `~/dev/BRIEF.md` on the Mac mini.

Docs in this folder:

- [development.md](./development.md): building on the Mac mini, measurements, screenshots,
  the Paseo test daemon.
- [paseo-protocol.md](./paseo-protocol.md): the daemon protocol as `crates/paseo_client` uses
  it, with what was verified against a real daemon.
- [agent-sessions.md](./agent-sessions.md): how Paseo agents are shown with Zed's thread UI.
- [terminals.md](./terminals.md): how Paseo terminals are shown with Zed's terminal view.
- [todo.md](./todo.md): open work, in priority order.

This folder is outside `docs/src/`, so Zed's mdBook and docs automation ignore it.

## Repository layout and remotes

- Local only for now: there is no GitHub fork yet. The clone's remote for Zed is named
  `upstream` (not `origin`) so that a fork can be added as `origin` later without renaming.
- Fork code goes into new crates and fork tooling into `script/pazedo/`. Crates so far:
  `crates/paseo_client` (daemon protocol client; no gpui dependency) and `crates/paseo_ui`
  (the Paseo workspace sidebar, the store behind it, Paseo agent sessions shown with Zed's
  thread UI and Paseo terminals shown with Zed's terminal view; see
  [agent-sessions.md](./agent-sessions.md) and [terminals.md](./terminals.md)).
  Upstream files are changed only where there is no seam, and every such change is listed
  under [Upstream touch points](#upstream-touch-points).

## Syncing with upstream

Upstream moves fast (on the order of 100 commits a week, unverified), so sync often and keep
the diff against upstream small.

Procedure (merge, not rebase):

```sh
git fetch upstream
git switch main
git merge upstream/main          # resolve conflicts in the touch points listed below
cargo build -p zed               # then run it and check the fork's UI on screen
```

Why merge rather than rebase: several agents may work in worktrees branched off `main` at the
same time; rebasing `main` would rewrite the commits they are based on. Merging also resolves
each conflict once, instead of replaying every fork commit on every sync.

Do it at least weekly. When a merge conflicts in an upstream file, update the touch point
entry below if the seam moved.

Merges so far:

| Date | Upstream | Commits | Conflicts | Notes |
| --- | --- | --- | --- | --- |
| 2026-11-15 | `596dfdcf32` | 9 | none (`Cargo.toml`, `Cargo.lock` merged automatically) | Included an ACP SDK update (5.3.0) that reworked `acp_thread` and `agent_ui`'s conversation view, which the Paseo agent tabs reuse; the fork built without changes, its tests passed, and a mock agent ran in the dev build. The fork then changed 13 upstream files by about 290 lines (plus `Cargo.lock`), next to about 14,200 lines in its own crates. |

## Upstream touch points

Every upstream file the fork changes, with the reason. Keep this list current; it is what
makes syncing tractable.

| File | Change | Why |
| --- | --- | --- |
| `Cargo.toml` | `crates/paseo_client`, `crates/paseo_ui`, `crates/type_ahead` in `members` and `[workspace.dependencies]` | Every new crate. Conflicts here are trivial: keep both sides, sorted. |
| `crates/zed/Cargo.toml` | `paseo_ui` dependency | For the sidebar registration below. |
| `crates/zed/src/zed.rs` | In `initialize_workspace`, register `paseo_ui::PaseoSidebar` instead of `sidebar::Sidebar` and open it; drop the `use sidebar::Sidebar` import. In `initialize_panels`, add `paseo_ui::PaseoPanel`. `ensure_agent_panel_for_workspace` returns early while `paseo_ui::REPLACES_ZED_AGENT_PANEL` is set | The Paseo sidebar replaces the threads sidebar; the Paseo panel (agent and terminal tabs in the left dock) replaces Zed's agent panel. The `sidebar` crate stays a dependency (`dump_workspace_info` is still used). The early return keeps Zed's agent-panel code compiled and untouched, so upstream changes to it merge cleanly. |
| `crates/ui/src/components/ai/thread_item.rs` | `ThreadItem::trailing_metadata`, an optional element at the end of the metadata row | The Paseo sidebar shows a workspace's pull request and CI state there (icons, not just text). Additive: unset, the row renders as before. |
| `crates/settings_content/src/settings_content.rs` | `PaseoSettingsContent` (`endpoint`, `name`, `ssh`, `daemons`, `dock`) and `PaseoDaemonContent`, a `paseo` field in `SettingsContent`, and `paseo` in its `flattened_deserialize!` list | Zed's settings are one typed struct, so the `paseo` section of settings.json has to be declared there. Additive. |
| `crates/settings/src/vscode_import.rs` | `paseo: None` in the `SettingsContent` it builds | The struct literal lists every field. One line. |
| `crates/zed/src/main.rs` | Read the data directory from `PAZEDO_USER_DATA_DIR` when `--user-data-dir` is not given, before the `--crash-handler` branch | Development builds (`script/pazedo/run-dev`) keep their data apart from Pazedo.app. They start through an app bundle, which cannot pass arguments, and a launcher script that adds them hides the app's windows from accessibility clients, so `open --env` sets the variable instead. The crash handler subprocess inherits the variable and writes to the logs directory, so the directory is set before it runs. |
| `crates/zed/Cargo.toml` | The binary is `pazedo` (`[[bin]] name`, `default-run`) | `main.rs` checks at compile time that the binary's name matches `paths::APP_NAME`. |
| `crates/paths/src/paths.rs` | `APP_NAME` is "Pazedo"; `state_dir`, `temp_dir` and `logs_dir` return `state`, `cache` and `logs` inside a custom data directory | The name gives Pazedo Zed's folder layout under its own name (`~/.config/pazedo`, `~/Library/Application Support/Pazedo`, …), as upstream's comment asks of forks. Upstream keeps state, cache and logs outside a `--user-data-dir`, so dev builds would share them with Pazedo.app. One constant and three early returns. |
| `crates/remote/src/transport.rs`, `crates/remote/src/transport/ssh.rs` | `bundled_remote_server` finds a remote server inside the app (`Contents/Resources/remote_server/<os>-<arch>.gz` and `version`); on the dev channel `ensure_server_binary` names the remote binary after that version, uploads the bundled server when the host lacks it, and deletes pazedo servers not uploaded for a week | The dev channel has no servers to download and only debug builds build one, so the release app would need servers copied to every host by hand. Naming the binary after the build makes a new app replace an old server. Additive: without a bundled server, the dev channel behaves as upstream. |
| `crates/agent_ui/src/thread_metadata_store.rs` | `handle_conversation_event` returns early for threads whose agent id is `paseo` | Zed records every open thread in its own thread database; Paseo already owns these agents' history, and Zed could not reopen the entries. Five lines. |
| `crates/terminal/src/terminal.rs` | `ForwardedInput`, a `forwarded_input_tx` field, `Terminal::forward_input`, two `else if` branches (in `write_to_pty` and the resize handler) and a test | A display-only terminal had nowhere to send keystrokes and resizes; with this, a Paseo terminal stream drives Zed's terminal view. About 30 lines plus the test, all additive. |
| `crates/settings_content/src/workspace.rs`, `crates/project_panel/src/project_panel_settings.rs`, `crates/settings/src/vscode_import.rs` | `project_panel.open_files_on_click` (`ProjectPanelOpenFilesOnClick`: `always`, `when_selected`), its field in `ProjectPanelSettings`, and `None` in the VS Code import's struct literal | A file opens on the second click rather than the first. Not in the settings UI, which would need a renderer for the enum in `settings_ui` as well. |
| `crates/project_panel/src/project_panel.rs`, `crates/project_panel/Cargo.toml` | In the entry's click handler, a branch before opening a file: with `when_selected`, a single click on a file that isn't selected only selects it. Type-to-find: `mod type_to_find` and a `type_to_find` field; `render` ends a search the selection left and adds the key listener and the search strip; `dispatch_context` adds `type_ahead` during a search; `cancel` and `open_permanent` end it first; a `type_ahead` dependency. Each hook has a `// Pazedo:` comment. | The search needs the panel's private state (visible entries, selection), so it lives in the crate, in its own file (`type_to_find.rs`, with its test), and the upstream file only gets one-line hooks. |
| `crates/title_bar/src/title_bar.rs` | An `extra_items` field, `TitleBar::add_item` and `TitleBar::workspace`, and the extra items rendered before the sign-in button and user menu | The AI usage indicator (`paseo_ui::usage`) is added to each title bar from outside, through `cx.observe_new::<TitleBar>`, and picks its daemon from the title bar's workspace. Marked `// Pazedo:`. |
| `crates/zed/src/main.rs` (init) | `paseo_ui::init(cx)` after `project_panel::init` | Registers the title bar observer before the first window opens. |
| `assets/settings/default.json` | `title_bar.show_sign_in` defaults to `false`; `project_panel.open_files_on_click` defaults to `when_selected` (Zed's behaviour is `always`) | Pazedo doesn't use a Zed account, so the title bar's Sign In button is hidden; `"title_bar": { "show_sign_in": true }` brings it back. |
| `assets/keymaps/default-macos.json` | Entries at the end: `ProjectPanel && type_ahead` (backspace and space `null`, enter `project_panel::OpenPermanent`) for type-to-find, and bindings of cmd-n to `paseo_sidebar::NewWorkspace` and cmd-g to `paseo_sidebar::ToggleGrouping` in `PaseoSidebar`, and to `paseo_panel::NewAgentOrTerminal` in `PaseoPanel`, `PaseoPanel > AcpThread` and `PaseoPanel > Terminal` | Agent and terminal tabs bind cmd-n themselves (new thread, new local terminal); an entry that matches as deep and comes later wins, which a binding registered in code cannot do (Zed clears those when it reloads the keymap). Keep them at the end of the file when merging. |
| `README.md` | Two `> [!IMPORTANT]` lines at the top | Required by upstream's `.rules` (symlinked as `CLAUDE.md`) whenever an agent modifies source files: a reminder that a human must review the change. Only a human removes them. |

