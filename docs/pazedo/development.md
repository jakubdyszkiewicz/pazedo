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

Disk: each worktree has its own `target/` (24 GB+). Build in at most one or two worktrees at a
time and watch `df -h`.

## Running a dev build

```sh
cargo build -p zed
script/pazedo/run-dev ~/dev/pazedo-sandbox     # stops a running dev build first
```

`run-dev` starts the build with `PAZEDO_PASEO_ENDPOINT=127.0.0.1:6868`, the test daemon, unless
the variable is already set. pazedo connects to Paseo only when that variable is set, so a
plain `target/debug/zed` shows "Not connected" in the sidebar and never reaches the real daemon.

The Paseo sidebar only renders while Zed's AI features are enabled (`MultiWorkspace` hides the
sidebar when `disable_ai` is set or the agent is disabled).

Open a project other than this repository: a dev build that opens the Zed repo itself runs
rust-analyzer with `cargo run`'s environment and invalidates the build cache. Errors in
`~/Library/Logs/Zed/Zed.log` about local model providers (LM Studio, Ollama, llama.cpp), a
missing Anthropic key or `canonicalizing "crates/grammars/src"` (the binary was not started
from the repo root) are expected.

## Screenshots

Agents on this machine must check UI work on screen, not only compile it:

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
