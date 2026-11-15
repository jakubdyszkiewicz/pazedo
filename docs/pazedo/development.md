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
| `target/debug/zed` binary | 1.16 GB |
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
app 357 MB. The app is ad-hoc signed, on the dev channel (no auto-update), and keeps its data
in `~/Library/Application Support/Pazedo`, so it can run next to a dev build. The daemon is a
setting in the app's own settings file, `~/Library/Application Support/Pazedo/config/settings.json`
(see "Connecting to a daemon" below).
First launch asks whether to trust the opened project (Zed's restricted mode), because the
data directory is new.

## Running a dev build

```sh
cargo build -p zed
script/pazedo/run-dev ~/dev/pazedo-sandbox     # stops a running dev build first
```

`run-dev` copies the binary into `.pazedo-dev/PazedoDev.app` (git-excluded through
`.git/info/exclude`) and starts that. Not `target/`: an app started from the external disk
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

The Paseo sidebar only renders while Zed's AI features are enabled (`MultiWorkspace` hides the
sidebar when `disable_ai` is set or the agent is disabled).

Open a project other than this repository: a dev build that opens the Zed repo itself runs
rust-analyzer with `cargo run`'s environment and invalidates the build cache. Errors in
`~/Library/Logs/Zed/Zed.log` about local model providers (LM Studio, Ollama, llama.cpp), a
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
