# Developing pazedo on the Mac mini

Machine: Mac mini M1, 8 cores, 16 GB RAM, 256 GB internal SSD, macOS 27.0.1.

## Toolchain

Follows `docs/src/development/macos.md`:

- Xcode 27.2 from the App Store, selected with
  `sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer`, license accepted,
  `xcodebuild -runFirstLaunch` and `xcodebuild -downloadComponent MetalToolchain` run.
  The command line tools alone are not enough: they have no `metal` compiler, and gpui compiles
  its shaders at build time.
- `brew install rustup cmake`. Homebrew's rustup is keg-only, so put it on `PATH`:
  `export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"`. The toolchain version
  comes from `rust-toolchain.toml` (1.98.1) and installs on the first `cargo` call.

Without Xcode, `cargo build -p zed --features gpui_platform/runtime_shaders` builds anyway: the
shaders are then compiled when the app starts. Switching that feature on or off only rebuilds
`gpui_apple`, `gpui_macos`, `gpui_platform` and `zed`.

## Build measurements

Measured 2026-11-14 at upstream `99c890d39d`, `cargo build -p zed` (debug, default `-j 8`),
with `/usr/bin/time -l`:

| | Measured |
| --- | --- |
| Cold build | 10 min 10 s wall, 38 min user CPU |
| Peak memory of one process (the link) | 6.1 GB RSS |
| `target/` after one debug build | 24 GB |
| `target/debug/zed` binary (now `target/debug/pazedo`) | 1.16 GB |
| `~/.rustup` + `~/.cargo` | 2.3 GB + 1.5 GB |
| Rebuild after enabling compile-time Metal shaders | 27 s (4 crates) |

The linker warns `__eh_frame section too large (max 16MB) to encode dwarf unwind offsets in
compact unwind table`; harmless for development.

Disk: the internal SSD is too small for Zed's build output (a day of debug builds grew
`target/debug` to 108 GB, 60 GB of it `incremental/`). Since 2026-11-14 `target` is a symlink
to `/Volumes/dev/pazedo-target` on the external SSD (APFS, USB, about 94 MB/s for many small
files). The symlink keeps absolute paths unchanged, so cargo's fingerprints stayed valid when
the directory moved. Builds fail while the disk is unplugged. `rm -rf target/debug/incremental`
is a safe way to reclaim space: it is only cache. Each worktree has its own `target/`; point it
at the external disk too.

## Release app

```sh
script/pazedo/bundle --install      # release build, target/release/Pazedo.app, ~/Applications
script/pazedo/bundle --skip-build --install   # re-bundle only
```

Measured 2026-11-14 (first release build, `target` on the external disk): 20 min 41 s wall,
2 h 9 min user CPU, peak RSS 8.2 GB (the thin-LTO link), `target/release` 18 GB, binary 374 MB,
app 357 MB. The app is ad-hoc signed and on the dev channel (no auto-update).

`paths::APP_NAME` is "Pazedo" (and so the binary is `pazedo`, which a compile-time check in
`crates/zed/src/main.rs` requires), so the app has Zed's folder layout under its own name and
shares nothing with an installed Zed: settings, keymap and themes in `~/.config/pazedo`, data
(databases, extensions, language servers) in `~/Library/Application Support/Pazedo`, logs in
`~/Library/Logs/Pazedo/Pazedo.log`, cache in `~/Library/Caches/Pazedo`, state in
`~/.local/state/Pazedo`. The daemon is a setting in `~/.config/pazedo/settings.json` (see
"Connecting to a daemon" below). Until 2026-11-15 the app instead set `PAZEDO_USER_DATA_DIR`
to `~/Library/Application Support/Pazedo` in its `Info.plist`, which put its settings in
`…/Pazedo/config`; move them to `~/.config/pazedo` when updating from such a build:

```sh
mkdir -p ~/.config/pazedo
mv ~/Library/Application\ Support/Pazedo/config/* ~/.config/pazedo/
```

First launch asks whether to trust the opened project (Zed's restricted mode), because the
data directory is new.

## Remote server for SSH projects

Opening a folder on another machine over SSH (Zed's remote development) needs Zed's remote
server on that machine, built from the same source as the app. Official Zed downloads its
server from zed.dev; pazedo is on the dev channel, which has nothing to download.

`script/pazedo/bundle` builds servers for Linux x86_64 and Apple Silicon macOS
(`PAZEDO_REMOTE_SERVER_TARGETS` changes the list) with `script/pazedo/remote-server` and puts
them in `Pazedo.app/Contents/Resources/remote_server` (`linux-x86_64.gz`, `macos-aarch64.gz`
and a `version` file: the commit, plus a timestamp when the tree had uncommitted changes).
Connecting to a host, the app uploads the matching server as
`~/.zed_server/zed-remote-server-dev-<version>` unless it is already there, so every new build
replaces the server on its next connection, and deletes pazedo servers that no build has
uploaded for a week (`crates/remote/src/transport.rs`, `bundled_remote_server`). Linux servers
are static musl binaries cross-compiled with zig and cargo-zigbuild, like Zed's own dev setup;
macOS servers build natively and are ad-hoc signed after stripping.

A debug build has no bundled servers: it builds one from source on connect (Zed's default for
debug builds) or, with `ZED_BUILD_REMOTE_SERVER=false`, uses
`~/.zed_server/zed-remote-server-dev-build` on the remote; the header of
`script/pazedo/remote-server` has the commands to install one there.

Measured 2026-11-15 on the Mac mini (M1, 8 cores, 16 GB): first build 12 min 10 s wall, 57 min
user CPU, peak RSS 7.6 GB, `target/remote_server` 5.5 GB; the binary is 679 MB with debug info,
90 MB stripped, 36 MB gzipped.

## Running a dev build

```sh
cargo build -p zed
script/pazedo/run-dev ~/dev/pazedo-sandbox     # stops a running dev build first
```

`run-dev` copies the binary into `.pazedo-dev/PazedoDev.app` (git-excluded through
`.git/info/exclude`) and starts that, with the data directory `.pazedo-dev/data`
(`PAZEDO_USER_DATA_DIR`, like `--user-data-dir`: settings in its `config`, logs in its `logs`),
so it shares nothing with Pazedo.app or Zed. Not `target/`: an app started from the external disk
blocks until someone answers macOS's "access files on a removable volume" prompt. And not
outside the repository: debug builds find their assets by walking up from the executable to
the directory with `.git`. A debug build that panics still reads debug info from `target/`
for the backtrace, which triggers that prompt.

## Connecting to a daemon

pazedo connects to the daemon named in settings.json, and reconnects when the setting changes:

```json
"paseo": { "endpoint": "100.64.0.1:6767" }
```

The endpoint is `host:port`, `ws://host:port/ws`, or a Unix socket path. Without it pazedo does
not connect and the sidebar says so. The `PAZEDO_PASEO_ENDPOINT` environment variable overrides
the setting; `run-dev` uses that so a dev build, which shares `~/.config/zed` with any other dev
build, always talks to the test daemon. The setting lives in Zed's `SettingsContent`
(`crates/settings_content`, an upstream touch point); `paseo_ui::PaseoSettings` reads it.

`run-dev` starts the build with `PAZEDO_PASEO_ENDPOINT=127.0.0.1:6868`, the test daemon, and
`PAZEDO_AGENT_PROVIDER=mock` / `PAZEDO_AGENT_MODEL=one-minute-stream` for agents created from
Zed, unless the variables are already set. It also forwards `PAZEDO_AGENT_HISTORY_LIMIT` and
`RUST_LOG` when set (`open` passes no other environment). With `PAZEDO_AGENT_PROVIDER` set, the
sidebar's + menu lists that provider first.

### Daemon on another machine

For a daemon on another machine, `ssh` names the SSH host its folders are on:

```json
"paseo": {
  "endpoint": "100.64.0.2:6767",
  "ssh": "root@dev-box"
}
```

`ssh` takes what Zed's "Connect to SSH server" takes (`user@host`, `ssh -p 2222 user@host`); a
matching `ssh_connections` entry adds its nickname, arguments and port forwards. Workspaces
then open as Zed remote projects through `MultiWorkspace::find_or_create_workspace`, behind
Zed's connection dialog, and all workspaces of the daemon share one SSH connection. The panel
and the sidebar match a window to a Paseo workspace only when its project is on the daemon's
host (`store::is_on_daemon_host`), so a local folder never shows a remote workspace of the same
path. Add Project picks a folder from the daemon's `directory_suggestions_request` instead of
the native dialog. The remote needs pazedo's remote server (see "Remote server for SSH
projects" above). With `PAZEDO_PASEO_ENDPOINT` set, `PAZEDO_PASEO_SSH` replaces the setting;
`run-dev` forwards it, and `ZED_BUILD_REMOTE_SERVER=false` keeps a debug build from building
its own remote server:

```sh
PAZEDO_PASEO_ENDPOINT=100.64.0.2:6767 PAZEDO_PASEO_SSH=root@dev-box \
  ZED_BUILD_REMOTE_SERVER=false script/pazedo/run-dev
```

Verified 2026-11-15 against the dev box (Ubuntu 24.04, x86_64, root's shell is fish):
workspaces of a checkout and of a Paseo worktree opened over SSH with their remote files and
agents; gopls ran on the box (hover worked in a trusted project); the folder picker listed
`/root/dev`.

### Daemon password

A daemon with a password (`paseo daemon set-password`, then `paseo daemon restart`; or
`PASEO_PASSWORD` in its environment) refuses Pazedo's `hello` until it sends one. Pazedo then
shows the daemon as "Password required", with Enter Password… in the sidebar. The typed
password is tried at once and saved only after the daemon accepts it; a refused one shows "…
rejected the password" and the prompt again. The client does not retry refused credentials on
its own (`Event::Rejected` in `paseo_client`); it waits for `Client::set_auth` and
`Client::reconnect_now`.

On connecting, Pazedo sends the saved password for the endpoint, else, for a daemon on this
machine (a Unix socket, `localhost`, a loopback address or an address of one of this machine's
interfaces, such as its own Tailscale address), the daemon's local credential from
`$PASEO_HOME/local-credential` (default `~/.paseo`), which the daemon accepts like its password,
so Pazedo needs no password for the Mac mini's own daemon. Passwords never go to settings.json:
release builds keep them in the macOS Keychain under `paseo://<endpoint>`; debug builds use Zed's
development credentials file (`<config dir>/development_credentials`), as Zed's own debug builds
do, because every rebuild is signed differently and the Keychain would ask for access each
time. Pazedo.app is ad-hoc signed per build too, so after an update macOS may ask once whether
it may read the saved password; Always Allow lasts until the next build.

The password travels in plain text inside the WebSocket (`ws://`); only use it over Tailscale,
SSH or another encrypted link.

Verified 2026-11-15 in a dev build against the test daemon with a password: the sidebar asked
for it, a wrong one was refused and not saved, the right one connected and was saved, a restart
connected with the saved password, and with no saved password and `PASEO_HOME` set to the test
daemon's home it connected with the local credential. The log shows only the two refusals,
never the password. The Keychain path of release builds was not exercised here.

### Grouping

The sidebar header's group button (list-tree icon), or cmd-g in the focused sidebar
(`paseo_sidebar::ToggleGrouping`), switches between grouping by project (the default) and by
status, as Paseo's app does: Needs input, Failed, Ready to review, Working,
Done, most recent status change first, across all daemons. Each status has the same
collapsible header as a project. Status rows show their branch, project and (with several
daemons) daemon. The choice is kept in Zed's key-value store
(`pazedo_sidebar_grouping`), per data directory. cmd-g is bound only in the sidebar, as
upstream's threads sidebar binds it; elsewhere it stays Find Next. Verified 2026-11-16 in a dev
build: cmd-g in the focused sidebar switched to status groups and back.

Grouping by project lists the projects of all daemons together, by name. As in Paseo's app, a
repository on several daemons is one project: daemons give a project a `projectKey` from its
git remote (`remote:github.com/owner/repo`; a folder without a remote gets a key of its daemon
and path), and projects with the same key merge, unless one daemon has two projects with that
key. The merged project takes its name from the first daemon in the settings. Once the listed
projects span several daemons, each workspace row names its daemon. New Workspace on a project
of several daemons first asks which machine the workspace goes on. Verified 2026-11-15 with a
scratch clone of webapp (only its remote URL) on the test daemon, merged with the dev box's
webapp, and a new workspace created through the machine menu on the test daemon.

### Panel position

The Paseo panel's dock menu (right-click its status bar icon: Dock Left, Right or Bottom) writes
`"paseo": { "dock": "right" }` to settings.json, and the panel docks there from then on, also
after a restart. Zed's dock only moves a panel when the settings change, which is why Zed's own
panels keep their position in settings too; until 2026-11-15 the Paseo panel kept it in memory,
and the menu did nothing. Verified 2026-11-15 in a dev build: Dock Right moved the panel and its
icon to the right and wrote the setting, and a restart kept it there.

### Keyboard

The sidebar has the key contexts `PaseoSidebar` and `menu`, so Zed's list keys work in it once
it has focus: up and down move the selection (outlined), home and end jump to the ends, enter
opens the selected workspace or collapses the selected project or status section, left
collapses a section or moves from a workspace to its section, right expands one or moves into
it, escape hands focus back, and cmd-n starts a new workspace in the project of the selected row (a
workspace or a project; the open workspace's project otherwise), like the project's + button,
including its machine menu for a project on several daemons (`paseo_sidebar::NewWorkspace`,
bound in Zed's default macOS keymap). A workspace created from the sidebar opens and then starts
renaming, with the daemon's name (its worktree's) selected, so typing names it and escape keeps
it. Verified 2026-11-15 in a dev build: cmd-n on a workspace row opened its project's picker,
a local checkout was created and renamed by typing. Typing letters in the focused sidebar searches the workspaces'
titles (letters in order, ignoring case; runs of letters, word starts and early matches rank
higher) and selects the best match without hiding the others, scrolling to it. The typed
letters show at the bottom of the sidebar, red when nothing matches; backspace takes one back,
enter opens the selected workspace, escape ends the search, and up and down end it and move on
from the found row. Verified 2026-11-15 in a dev build. The search itself (matching, the
query, the strip) is the `type_ahead` crate, shared with the project panel. Focusing the sidebar with `multi_workspace::FocusWorkspaceSidebar`
(cmd-alt-; by default) selects the open workspace's row. The Paseo panel has the key context
`PaseoPanel`. Focusing it (`paseo_panel::ToggleFocus`, or switching to an agent tab) puts the
cursor in the agent's message box, as activating a thread does in Zed's agent panel. Cmd-n
anywhere in the panel, including an agent's message box and a terminal, opens its New Agent or
Terminal menu (`paseo_panel::NewAgentOrTerminal`, bound in Zed's default macOS keymap), or the
New Agent menu of an empty panel, which has no tab bar; outside
the panel cmd-n still makes a new file. Verified 2026-11-15 in a dev build: cmd-n from an agent's
message box opened the menu, and escape returned to the message box. Neither needs changes to Zed's keymaps: the list keys are bound globally (up,
down, enter, escape) and in the `menu` context (left, right).

cmd-shift-0 (`paseo_sidebar::RevealActiveWorkspace`, bound in the `Workspace` and `PaseoSidebar`
contexts) reveals the open workspace, like the project panel's reveal of the open file: it opens
and focuses the sidebar (as `FocusWorkspaceSidebar` does, so escape goes back), expands the
project or status section the workspace is under, selects its row and scrolls to it. The
sidebar notes, while rendering, which collapsed section each hidden workspace is under, so
expanding follows the same grouping as the list. The window-wide handler is registered on every
Zed workspace by `paseo_ui::init` and waits for the workspace's update to end (opening the
sidebar updates every workspace). Covered by
`test_revealing_the_open_workspace_expands_its_project`; not yet tried in a running build.

That allows JetBrains-style toggles, where one key focuses a panel and, pressed in it, hides it.
In the user's `keymap.json` (Pazedo.app's is `~/.config/pazedo/keymap.json`):

```json
[
  {
    "context": "Workspace",
    "bindings": {
      "cmd-2": "multi_workspace::FocusWorkspaceSidebar",
      "cmd-3": "paseo_panel::ToggleFocus"
    }
  },
  { "context": "PaseoSidebar", "bindings": { "cmd-2": "multi_workspace::ToggleWorkspaceSidebar" } },
  { "context": "PaseoPanel", "bindings": { "cmd-3": "workspace::CloseActiveDock" } }
]
```

The first block needs the `Workspace` context: Zed binds cmd-1 to cmd-9 there
(`workspace::ActivatePane`), and a binding without a context loses to it. The sidebar is
outside the `Workspace` element, hence its own context. Verified 2026-11-15 in a dev build with
this keymap: cmd-2 focused the sidebar on the open workspace, the arrows moved, collapsed and
expanded, cmd-2 in the sidebar hid it, cmd-3 focused the panel and cmd-3 in it closed the dock.

### Project panel

Typing in the focused project panel finds an entry the same way: it selects the visible entry
(the expanded part of the tree) whose name matches best, shows the typed letters at the panel's
bottom, and leaves the tree as it is. Backspace takes a letter back and space types one (the
keymap's `ProjectPanel && type_ahead` entry unbinds trash and open while a search is on), enter
opens the match (`project_panel::OpenPermanent`: a file in a tab of its own, with the focus; a
folder opens or closes), escape ends the search, and moving the selection (arrows, a click) or
leaving the panel ends it. The code is in `crates/project_panel/src/type_to_find.rs`, hooked into
`project_panel.rs` in a few places. Verified 2026-11-16 in a dev build: "rea" selected
README.md, backspace shortened the search without trashing the file, and "ma" then enter opened
main.rs.

A click on a file selects it, and a click on the selected file (or a double click) opens it:
`"project_panel": { "open_files_on_click": "when_selected" }`, Pazedo's default; `"always"` opens
on every click, as in Zed. Folders still open and close on the first click. Verified 2026-11-16
in a dev build: a click on main.rs only selected it, the next click opened it. Synthetic clicks
need a click state of 1 (`script/pazedo/click.swift` sets it), or apps see a click count of 0.

### Images in agent messages

Zed's agent view shows markdown images from absolute paths and `http(s)` URLs, not `file://`
URLs, which is what Paseo writes for a provider's images (see paseo-protocol.md), so they showed
as their alt text ("Image"). `paseo_ui::agent_images` rewrites those links in assistant messages,
live and replayed: to the file itself when it exists on this machine (a daemon on this Mac), else
to a copy in `<cache>/paseo-images`, which it reads through the daemon (`ReadFile`) and then
redraws the windows. Verified 2026-11-16 in a dev build with a mock agent on the test daemon
(the image rendered from its local file); the copy through the daemon was checked against the
real daemon with `paseo_debug raw` but has not run in a build against a daemon on another
machine.

### AI usage in the title bar

Every window's title bar shows the usage of the AI subscriptions on the right, as the Paseo app
does at the bottom of its sidebar: each account's icon (Claude, OpenAI for Codex, …), then for
each of its summary windows a meter, the percent left and the window's short label, e.g.
"✳ ▬ 71% 5h ▬ 59% wk ◎ ▬ 82% 5h ▬ 67% wk". Meters and percents turn orange or red with the
window's tone. A click opens every window with its reset time, the balances and details, which
daemon it came from and when, and Refresh (which skips the daemon's five-minute cache). The
usage comes from the daemon whose folders are on the window's machine, else the first daemon
with usage; each store reads it on connecting and every five minutes after
(`PaseoStore::read_usage`). Accounts without summary windows show only in the popover, and the
indicator is empty while no daemon reports usage. The code is `crates/paseo_ui/src/usage.rs`,
attached to each `TitleBar` through `TitleBar::add_item` (see docs/pazedo/README.md). Verified
2026-11-16 in a dev build against the test daemon: the title bar showed Claude and Codex, the
popover listed their windows, the Codex credits in red, "From My Mac", and Refresh
read them again.

### Several daemons

`daemons` lists several daemons; their workspaces are listed together (see "Grouping"). The
sidebar header says how many are connected: "Paseo" with a dot and "All online" (green), "2/3
online" (orange; red when none is connected or connecting), or with a single daemon its status
("Online", "Offline", …). Hovering it lists each daemon's status, and clicking it opens the Paseo
Daemons dialog. A daemon's problems (offline, a password needed, failed loading) show above the
workspaces, named after the daemon when there are several. The header's + adds a project: on the
one daemon, or with several on the one picked from its "Add Project On" menu (offline daemons
greyed out). Verified 2026-11-16 in a dev build: "Online" with the test daemon, "All online"
with it and the dev box, "2/3 online" with an unreachable third daemon, and + → dev box opened
the dev box's folder picker. `endpoint` and `ssh` at the top level are shorthand for one more
daemon, listed first:

```json
"paseo": {
  "daemons": [
    { "endpoint": "100.64.0.1:6767" },
    { "name": "dev box", "endpoint": "100.64.0.2:6767", "ssh": "root@dev-box" }
  ]
}
```

The Paseo Daemons dialog (the gear in the sidebar header, `paseo: manage daemons` in the
command palette, or Add Daemon… when none is configured) lists the daemons of settings.json with
their connection status, and adds, edits (endpoint, name, SSH host) and removes them, writing
settings.json where each one is: the top-level shorthand or its `daemons` entry. It checks the
endpoint and SSH host before saving and refuses an endpoint another daemon has. Removing a
daemon also deletes its saved password. With `PAZEDO_PASEO_ENDPOINT` set it says that the
variable wins. Verified 2026-11-15 in a dev build: a duplicate endpoint was refused, a daemon was
added, edited and removed, and settings.json ended as it started.

`name` defaults to the daemon's host name; the dialog's Name field sets it. The top-level
shorthand takes a `name` too. Each daemon has its own `PaseoStore` and
`PaseoConnection`; `PaseoDaemons` keeps the list in step with the settings, keeping a daemon's
store (and connection) while its endpoint stays. Agent and terminal tabs hold their daemon's
store. A Zed workspace's panel shows the first daemon whose folders are on the workspace's
machine and that has a Paseo workspace for its folder. `PAZEDO_PASEO_ENDPOINT` replaces the whole
list with one daemon.

A daemon on the machine Pazedo runs on needs no `ssh`. Run on another Mac (e.g. the MacBook),
the Mac mini's daemon is remote too: its folders are on the mini, so it needs
`"ssh": "me@my-mac"`; the app uploads its macOS remote server there. The mini
needs Remote Login (System Settings, General, Sharing) and the MacBook's key in
`~/.ssh/authorized_keys`.

To try several daemons with a dev build without touching `~/.config/zed`, give it its own data
directory: write `<dir>/config/settings.json`, then start the bundle that `run-dev` made with
`open -n -a "$PWD/.pazedo-dev/PazedoDev.app" --env PAZEDO_USER_DATA_DIR=<dir> --env
ZED_BUILD_REMOTE_SERVER=false` (no `PAZEDO_PASEO_ENDPOINT`, which would replace the list).
Verified 2026-11-15 with the test daemon and the dev box side by side: both listed, a dev box
workspace opened over SSH and a local one in the same window, each panel showing its own
daemon's agents, and a new mock agent started from the local panel ran on the test daemon.

The Paseo sidebar only renders while Zed's AI features are enabled (`MultiWorkspace` hides the
sidebar when `disable_ai` is set or the agent is disabled).

Open a project other than this repository: a dev build that opens the Zed repo itself runs
rust-analyzer with `cargo run`'s environment and invalidates the build cache. Errors in the log
(`~/Library/Logs/Pazedo/Pazedo.log` for Pazedo.app, `logs/Pazedo.log` in a dev build's data
directory) about local model providers (LM Studio, Ollama, llama.cpp), a
missing Anthropic key or `canonicalizing "crates/grammars/src"` (the binary was not started
from the repo root) are expected.

## Testing UI work

Three layers, cheapest first. UI work that was only compiled, never looked at, is not done.

1. **gpui tests** (`cargo test -p paseo_ui`): real views on a fake platform with a deterministic
   scheduler and `FakeFs`, as Zed tests its own UI. Use them for behaviour: what a click does,
   what the store does with each daemon message. Example: selecting a Paseo workspace activates
   the Zed workspace for its directory.
2. **Off-screen screenshots** (`cargo run -p paseo_ui --example sidebar_screenshots --features
   test-support`): renders the sidebar in a real Zed window (title bar, workspace, sidebar) with
   gpui's `VisualTestAppContext`, the same machinery as Zed's `zed-visual-test-runner`: real
   Metal rendering into a texture, no visible window, no Screen Recording permission, nothing
   on the user's screen. The store is fed scripted fixtures, one scenario per state
   (connected with every workspace and agent status, disconnected, not configured, sync
   error). PNGs go to `target/pazedo-screenshots/`. Add a scenario for every new visual state
   and look at the images. Verified 2026-11-14; it caught message banners that did not wrap.
   No baselines are compared yet.
   With `PAZEDO_PASEO_ENDPOINT=127.0.0.1:6868 PAZEDO_SCREENSHOT_AGENT=<agent id>` (or `new`)
   it also opens that agent's session from the live test daemon in a tab and captures it
   (`agent-session.png`); `PAZEDO_SCREENSHOT_SEND="text"` types and sends a message
   (`agent-session-sent.png`), and `PAZEDO_SCREENSHOT_CLICK=x,y` clicks a point in window
   coordinates, e.g. a permission button (`agent-session-clicked.png`), and
   `PAZEDO_SCREENSHOT_EXTERNAL_SEND="text"` sends a message as another client would
   (`agent-session-external.png`). Real socket I/O runs
   while the deterministic scheduler is pumped, so this works with the screen locked. With
   `new`, set `PAZEDO_AGENT_PROVIDER=mock` (and a mock model) or a real agent is created.
   `PAZEDO_SCREENSHOT_TERMINAL=<terminal id>` (or `new`) attaches to a terminal of the
   sandbox's newest workspace instead, types `PAZEDO_SCREENSHOT_TYPE` (default
   `echo pazedo-$((6*7))`) and Enter, and prints the terminal's text
   (`terminal-session.png`, `terminal-typed.png`); `sidebar` only clicks
   `PAZEDO_SCREENSHOT_CLICK` (`terminal-sidebar.png`).
3. **The real app** against the test daemon (`script/pazedo/run-dev`, then
   `script/pazedo/screenshot`): for what fixtures cannot show, such as the live connection,
   updates arriving, and opening real directories. Needs the window in front (see below).

## Driving the running app

`script/pazedo/ui` raises the dev build and sends input (Accessibility permission for
Terminal.app, granted 2026-11-14):

```sh
script/pazedo/ui click 120 95      # coordinates as seen in a script/pazedo/screenshot image
script/pazedo/ui key 53            # a macOS key code (53 is Escape, 36 is Return)
script/pazedo/ui type "some text"  # types into the focused field
script/pazedo/ui move 200 92       # hover only, e.g. to reveal a row's + button
script/pazedo/ui right-click 100 488  # move there first: a context menu opens only on a hovered row
script/pazedo/ui scroll 200 600 -5 # scroll lines at a point; negative scrolls down
script/pazedo/ui frame             # window position and size in points
```

The scripts raise the window only when it is not already in front: raising an app that is in
front re-activates it, which closes any open popover menu. Clicks are real CoreGraphics mouse
events (`click.swift`). System Events' `click at` does
nothing in Zed because it goes through the accessibility tree, which gpui windows do not expose.
Verified 2026-11-14 against the test daemon: a mock agent started with `paseo-test run`
appeared in the running app with a spinner without a restart, and clicking its workspace row
opened `~/dev/pazedo-sandbox` (title bar, branch and project panel switched). The dev window
comes to the front whenever these scripts run.

The test daemon is a child of whichever shell started it. When the real daemon restarts, agent
shells die with it, and so does a test daemon started from one; start it again with the
command above.

## Screenshots of the running app

Agents on this machine can check the real app on screen:

```sh
script/pazedo/screenshot                      # newest target/*/zed -> /tmp/pazedo-screenshot.png
script/pazedo/screenshot -p <pid> -o out.png
```

It captures one window by id (`screencapture -l`, id found by `window_id.swift`), so windows
covering Zed don't matter and the rest of the screen is not captured. Zed also owns invisible
helper windows, so the script picks the largest on-screen one.

gpui does not redraw a window that is fully covered by other windows (it tracks
`NSWindowOcclusionState`), so `screencapture -l` of a covered window returns the last frame
drawn, which can be minutes old. Verified 2026-11-14: two captures 20 s apart were
byte-identical while the window sat behind Terminal, although the Paseo data had changed. The
script therefore raises the window with System Events first and warns when it cannot. macOS 14+
refuses `NSRunningApplication.activate` from another process, and `open` did not bring the
dev build to the front either.

Permission: macOS grants Screen Recording to the *responsible* app of a process. Agents run
under the Paseo daemon, and the daemon was started with `paseo daemon start` from Terminal.app,
so Terminal.app is the app that needs the permission (granted in System Settings › Privacy &
Security › Screen & System Audio Recording; Paseo.app is in the list too). After changing the
permission, restart the daemon from Terminal. Raising and clicking windows (System Events) also
needs Terminal.app in Privacy & Security › Accessibility. If the daemon is started another way,
`screencapture` fails with `could not create image from display`.

## Paseo test daemon

The real Paseo daemon (home `~/.paseo`, listening on the Tailscale address, port 6767) runs the
owner's actual agents, including the ones developing this fork. Never point tests or
experiments at it. Use the isolated test daemon instead:

| | Real daemon | Test daemon |
| --- | --- | --- |
| Home | `~/.paseo` | `~/.paseo-pazedo-test` |
| Listen | `100.64.0.1:6767` | `127.0.0.1:6868` (loopback only) |
| Mode | production | development (enables the `mock` providers) |
| Test project | — | `~/dev/pazedo-sandbox` (throwaway git repo) |

Always use `script/pazedo/paseo-test` instead of `paseo` for the test daemon. It:

- clears every `PASEO_*` variable first. Agents running under Paseo inherit `PASEO_HOME`,
  `PASEO_AGENT_ID` and similar, which would otherwise point the CLI at the real daemon or make
  commands default to the calling agent's workspace;
- passes `--home ~/.paseo-pazedo-test`;
- runs the CLI with `PASEO_NODE_ENV=development`. The bundled `paseo` launcher
  (`/Applications/Paseo.app/Contents/Resources/bin/paseo`) hard-codes `production`, and the
  daemon only registers its dev providers (`mock`, `mock-slow`) when the variable is
  `development`. Setting the variable around `paseo` has no effect, and `paseo daemon start`
  does not pass it to the worker either, so use `daemon run`.

Start it (once per boot; it keeps running after the shell exits):

```sh
script/pazedo/paseo-test daemon config set daemon.listen 127.0.0.1:6868   # first time only
script/pazedo/paseo-test daemon config set daemon.relay.enabled false    # first time only
(cd ~/.paseo-pazedo-test && nohup ~/dev/pazedo/script/pazedo/paseo-test daemon run \
  > ~/.paseo-pazedo-test/foreground.log 2>&1 < /dev/null &)
script/pazedo/paseo-test daemon status
script/pazedo/paseo-test provider ls      # should list mock and mock-slow
```

Use it:

```sh
script/pazedo/paseo-test run --no-wait --provider mock/ten-second-stream \
  --cwd ~/dev/pazedo-sandbox --title "mock smoke" "say hello"
script/pazedo/paseo-test ls
script/pazedo/paseo-test logs <agent-id>
script/pazedo/paseo-test daemon stop
```

The `mock` provider ("Mock Load Test") streams a realistic synthetic agent turn (text,
thinking, Read/Search tool calls) for a model-dependent duration: `ten-second-stream`,
`one-minute-stream`, `bursty-stream` (uneven bursts, for streaming smoothness),
`five-minute-stream` (default), `thirty-minute-stream`. `mock-slow` hangs during model
discovery, to test loading and timeout UI. Verified on 2026-11-14 with Paseo 0.15.4: a
`ten-second-stream` agent went from `running` to `idle` and its timeline was readable with
`logs`; the real daemon's PID and agent list were unchanged.

Use only the mock providers on the test daemon. The real providers would use the owner's
accounts, and OpenCode in particular is shared: both daemons log
`Using existing OpenCode v2 server http://127.0.0.1:49374`, which is the owner's own
`opencode serve --service` running under launchd. OpenCode sessions started from the test
daemon would appear in the owner's real OpenCode.

### Checking `paseo_client` against the test daemon

```sh
cargo test -p paseo_client                                   # unit and fake-daemon tests
cargo test -p paseo_client --test live_daemon_tests -- --ignored   # needs the test daemon
cargo run -p paseo_client --example paseo_debug -- workspaces
cargo run -p paseo_client --example paseo_debug -- agents
cargo run -p paseo_client --example paseo_debug -- watch 30          # live updates
cargo run -p paseo_client --example paseo_debug -- timeline <agent-id> 30
```

`paseo_debug` defaults to `127.0.0.1:6868`; `--endpoint` overrides it. While `watch` runs,
start a mock agent with `script/pazedo/paseo-test run ...` to see the updates arrive.

`paseo_client` takes its timer from `ConnectOptions::sleep`: Zed's clippy configuration bans
`smol::Timer` because gpui tests need gpui's deterministic executor. Code in Zed passes
`BackgroundExecutor::timer`; tests and the example pass `fake_daemon::smol_sleep()`.

Findings about the daemon from the installed bundle (Paseo 0.15.4, read from
`app.asar`, not from source):

- `daemon.listen` (or `PASEO_LISTEN`) accepts `host:port`, `/path/to/socket` or
  `unix:///path/to/socket`; the default is `127.0.0.1:6767`.
- `PASEO_PASSWORD` or `paseo daemon set-password` enables a password; the test daemon has none.
  To try one, add `"auth": {"password": "<bcrypt hash>"}` to `daemon` in
  `~/.paseo-pazedo-test/config.json` (`htpasswd -bnBC 12 "" <password>` makes one; change its
  `$2y$` prefix to `$2b$`) and restart the test daemon; remove it afterwards.
