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

## Upstream touch points

Every upstream file the fork changes, with the reason. Keep this list current; it is what
makes syncing tractable.

| File | Change | Why |
| --- | --- | --- |
| `Cargo.toml` | `crates/paseo_client`, `crates/paseo_ui` in `members` and `[workspace.dependencies]` | Every new crate. Conflicts here are trivial: keep both sides, sorted. |
| `crates/zed/Cargo.toml` | `paseo_ui` dependency | For the sidebar registration below. |
| `crates/zed/src/zed.rs` | In `initialize_workspace`, register `paseo_ui::PaseoSidebar` instead of `sidebar::Sidebar` and open it; drop the `use sidebar::Sidebar` import. In `initialize_panels`, add `paseo_ui::PaseoPanel`. `ensure_agent_panel_for_workspace` returns early while `paseo_ui::REPLACES_ZED_AGENT_PANEL` is set | The Paseo sidebar replaces the threads sidebar; the Paseo panel (agent and terminal tabs in the left dock) replaces Zed's agent panel. The `sidebar` crate stays a dependency (`dump_workspace_info` is still used). The early return keeps Zed's agent-panel code compiled and untouched, so upstream changes to it merge cleanly. |
| `crates/settings_content/src/settings_content.rs` | `PaseoSettingsContent` (`endpoint` and `ssh`), a `paseo` field in `SettingsContent`, and `paseo` in its `flattened_deserialize!` list | Zed's settings are one typed struct, so the `paseo` section of settings.json has to be declared there. Additive. |
| `crates/settings/src/vscode_import.rs` | `paseo: None` in the `SettingsContent` it builds | The struct literal lists every field. One line. |
| `crates/zed/src/main.rs` | Read the data directory from `PAZEDO_USER_DATA_DIR` when `--user-data-dir` is not given | `Pazedo.app` (`script/pazedo/bundle`) keeps its data apart from development builds. An app bundle cannot pass arguments, and a launcher script that adds them hides the app's windows from accessibility clients, so `Info.plist` sets the variable instead. Four lines. |
| `crates/agent_ui/src/thread_metadata_store.rs` | `handle_conversation_event` returns early for threads whose agent id is `paseo` | Zed records every open thread in its own thread database; Paseo already owns these agents' history, and Zed could not reopen the entries. Five lines. |
| `crates/terminal/src/terminal.rs` | `ForwardedInput`, a `forwarded_input_tx` field, `Terminal::forward_input`, two `else if` branches (in `write_to_pty` and the resize handler) and a test | A display-only terminal had nowhere to send keystrokes and resizes; with this, a Paseo terminal stream drives Zed's terminal view. About 30 lines plus the test, all additive. |
| `README.md` | Two `> [!IMPORTANT]` lines at the top | Required by upstream's `.rules` (symlinked as `CLAUDE.md`) whenever an agent modifies source files: a reminder that a human must review the change. Only a human removes them. |

