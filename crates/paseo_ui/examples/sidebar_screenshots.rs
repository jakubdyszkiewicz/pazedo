//! Renders the Paseo sidebar in a real Zed window, off screen, to PNG files.
//!
//!   cargo run -p paseo_ui --example sidebar_screenshots --features test-support
//!
//! Uses gpui's `VisualTestAppContext` like Zed's own visual test runner: real Metal rendering
//! into a texture, a deterministic scheduler, and no visible window, so it needs neither Screen
//! Recording permission nor an uncovered window. The store is fed scripted fixtures instead of
//! a daemon. Images go to `target/pazedo-screenshots/` (override with `PAZEDO_SCREENSHOT_DIR`).
//!
//! With `PAZEDO_PASEO_ENDPOINT` (the test daemon) and `PAZEDO_SCREENSHOT_AGENT=<agent id>` set,
//! it also renders that agent's session from the live daemon, opened in a tab with Zed's thread
//! UI, as `agent-session.png`. Real network I/O completes while the deterministic scheduler is
//! pumped, so this works with the screen locked.
//!
//! With `PAZEDO_SCREENSHOT_TERMINAL=<terminal id|new>` instead, it attaches to that terminal of
//! the sandbox workspace (or starts one), types a command into Zed's terminal view and renders
//! `terminal-session.png` and `terminal-typed.png`. `PAZEDO_SCREENSHOT_TERMINAL=sidebar` opens
//! nothing and only clicks `PAZEDO_SCREENSHOT_CLICK`, e.g. the sidebar's New Terminal entry.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("the screenshot runner uses the macOS Metal renderer");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    // Keeps Zed away from the user's real config and state directories.
    // SAFETY: no other threads exist yet.
    unsafe {
        std::env::set_var("ZED_STATELESS", "1");
    }
    env_logger::builder()
        .filter_level(log::LevelFilter::Warn)
        .init();
    screenshots::run()
}

#[cfg(target_os = "macos")]
mod screenshots {
    use anyhow::{Context as _, Result};
    use chrono::{Duration as ChronoDuration, Utc};
    use gpui::{
        AppContext as _, Bounds, Entity, VisualTestAppContext, WindowBounds, WindowHandle,
        WindowOptions, point, px, size,
    };
    use paseo_client::protocol::{Agent, ProjectSummary, ServerInfo, Workspace};
    use paseo_ui::{
        ConnectionStatus, DaemonConfig, Grouping, PaseoAgentItem, PaseoDaemons, PaseoSidebar,
        PaseoStore, PaseoTerminalItem,
    };
    use project::Project;
    use serde_json::{Value, json};
    use std::{
        path::PathBuf,
        sync::Arc,
        time::{Duration, Instant},
    };
    use workspace::{AppState, MultiWorkspace};

    struct Scenario {
        name: &'static str,
        status: fn() -> ConnectionStatus,
        directory: bool,
        sync_error: Option<&'static str>,
    }

    const SCENARIOS: &[Scenario] = &[
        Scenario {
            name: "sidebar-connected",
            status: connected,
            directory: true,
            sync_error: None,
        },
        Scenario {
            name: "sidebar-disconnected",
            status: || {
                ConnectionStatus::Disconnected(
                    "failed to connect to ws://127.0.0.1:6868/ws: Connection refused (os error 61)"
                        .into(),
                )
            },
            directory: true,
            sync_error: None,
        },
        Scenario {
            name: "sidebar-not-configured",
            status: || ConnectionStatus::NotConfigured,
            directory: false,
            sync_error: None,
        },
        Scenario {
            name: "sidebar-sync-error",
            status: connected,
            directory: false,
            sync_error: Some("fetch_workspaces_request timed out after 30s"),
        },
    ];

    pub fn run() -> Result<()> {
        let output_dir = std::env::var("PAZEDO_SCREENSHOT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pazedo-screenshots")
            });
        std::fs::create_dir_all(&output_dir)?;

        let project_dir = tempfile::tempdir()?.keep().canonicalize()?.join("pazedo");
        std::fs::create_dir_all(project_dir.join("src"))?;
        std::fs::write(project_dir.join("src/main.rs"), "fn main() {}\n")?;

        let mut cx = VisualTestAppContext::with_asset_source(
            gpui_platform::current_platform(false),
            Arc::new(assets::Assets),
        );
        cx.update(|cx| assets::Assets.load_fonts(cx))?;
        cx.update(|cx| settings::init(cx));
        let app_state = cx.update(init_app_state);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            workspace::init(app_state.clone(), cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            call::init(app_state.client.clone(), app_state.user_store.clone(), cx);
            title_bar::init(cx);
            // The runner loads no keymap; this is the default binding for sending a message.
            cx.bind_keys([gpui::KeyBinding::new(
                "enter",
                zed_actions::agent::Chat,
                Some("AcpThread > Editor"),
            )]);
        });
        cx.run_until_parked();

        let store = cx.update(|cx| {
            let store = cx.new(|cx| PaseoStore::new(DaemonConfig::default(), cx));
            PaseoDaemons::set_global_for_test(vec![store.clone()], cx);
            store
        });

        for scenario in SCENARIOS {
            let directory = project_dir.to_string_lossy().to_string();
            store.update(&mut cx, |store, cx| {
                let (projects, workspaces, agents) = if scenario.directory {
                    fixture(&directory)
                } else {
                    Default::default()
                };
                store.load_fixture((scenario.status)(), projects, workspaces, agents, cx);
                store.set_sync_error(scenario.sync_error.map(Into::into), cx);
            });

            let window = open_window(&project_dir, &app_state, &mut cx)?;
            let screenshot = cx.capture_screenshot(window.into())?;
            let path = output_dir.join(format!("{}.png", scenario.name));
            screenshot.save(&path)?;
            println!("{}", path.display());
            window.update(&mut cx, |_, window, _| window.remove_window())?;
            cx.run_until_parked();
        }

        // Two daemons side by side: this machine's, and one on a dev box reached over SSH.
        let remote_store = cx.update(|cx| {
            let remote_store = cx.new(|cx| {
                PaseoStore::new(
                    DaemonConfig {
                        name: Some("dev-box".into()),
                        endpoint: None,
                        ssh: Some("root@dev-box".into()),
                    },
                    cx,
                )
            });
            PaseoDaemons::set_global_for_test(vec![store.clone(), remote_store.clone()], cx);
            remote_store
        });
        store.update(&mut cx, |store, cx| {
            let (projects, workspaces, agents) = fixture(&project_dir.to_string_lossy());
            store.load_fixture(connected(), projects, workspaces, agents, cx);
            store.set_sync_error(None, cx);
        });
        remote_store.update(&mut cx, |store, cx| {
            let (projects, workspaces, agents) = fixture("/root/dev/webapp");
            store.load_fixture(connected(), projects, workspaces, agents, cx);
        });
        let window = open_window(&project_dir, &app_state, &mut cx)?;
        let screenshot = cx.capture_screenshot(window.into())?;
        let path = output_dir.join("sidebar-two-daemons.png");
        screenshot.save(&path)?;
        println!("{}", path.display());

        window.update(&mut cx, |multi_workspace, _, cx| {
            if let Some(sidebar) = multi_workspace
                .sidebar()
                .and_then(|sidebar| sidebar.to_any().downcast::<PaseoSidebar>().ok())
            {
                sidebar.update(cx, |sidebar, cx| sidebar.set_grouping(Grouping::Status, cx));
            }
        })?;
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, _| window.refresh())?;
        cx.run_until_parked();
        let screenshot = cx.capture_screenshot(window.into())?;
        let path = output_dir.join("sidebar-by-status.png");
        screenshot.save(&path)?;
        println!("{}", path.display());
        window.update(&mut cx, |multi_workspace, _, cx| {
            if let Some(sidebar) = multi_workspace
                .sidebar()
                .and_then(|sidebar| sidebar.to_any().downcast::<PaseoSidebar>().ok())
            {
                sidebar.update(cx, |sidebar, cx| {
                    sidebar.set_grouping(Grouping::Project, cx)
                });
            }
        })?;
        window.update(&mut cx, |_, window, _| window.remove_window())?;
        cx.run_until_parked();

        if let (Ok(endpoint), Ok(agent_id)) = (
            std::env::var(paseo_ui::ENDPOINT_ENV_VAR),
            std::env::var("PAZEDO_SCREENSHOT_AGENT"),
        ) {
            let path = output_dir.join("agent-session.png");
            live_agent_session(&endpoint, &agent_id, &app_state, &mut cx, &path)?;
            println!("{}", path.display());
        }
        if let (Ok(endpoint), Ok(terminal_id)) = (
            std::env::var(paseo_ui::ENDPOINT_ENV_VAR),
            std::env::var("PAZEDO_SCREENSHOT_TERMINAL"),
        ) {
            live_terminal(&endpoint, &terminal_id, &app_state, &mut cx, &output_dir)?;
        }
        Ok(())
    }

    fn live_terminal(
        endpoint: &str,
        terminal_id: &str,
        app_state: &Arc<AppState>,
        cx: &mut VisualTestAppContext,
        output_dir: &std::path::Path,
    ) -> Result<()> {
        let store = cx.update(|cx| {
            let store = cx.new(|cx| PaseoStore::new(DaemonConfig::local(endpoint), cx));
            PaseoDaemons::set_global_for_test(vec![store.clone()], cx);
            store
        });
        let directory = dirs_home().join("dev/pazedo-sandbox");
        pump(cx, Duration::from_secs(15), |cx| {
            store.read_with(cx, |store, _| {
                store.workspace_for_directory(&directory).is_some()
            })
        })
        .context("the test daemon did not list a sandbox workspace")?;
        let workspace_id = store.read_with(cx, |store, _| {
            store
                .workspace_for_directory(&directory)
                .map(|workspace| workspace.id.clone())
                .expect("checked above")
        });

        if terminal_id == "sidebar" {
            let window = open_window(&directory, app_state, cx)?;
            wait(cx, Duration::from_secs(2))?;
            click_from_env(window, cx)?;
            wait(cx, Duration::from_secs(3))?;
            let path = output_dir.join("terminal-sidebar.png");
            capture(cx, window, &path)?;
            println!("{}", path.display());
            return print_terminal(window, cx);
        }

        let (terminal_id, name) = if terminal_id == "new" {
            let create = store.update(cx, |store, cx| store.create_terminal(&workspace_id, cx));
            pump(cx, Duration::from_secs(10), |_| create.is_ready())?;
            let terminal = futures::FutureExt::now_or_never(create)
                .context("terminal creation did not finish")?
                .context("could not create a terminal")?;
            println!("created terminal {}", terminal.id);
            (terminal.id, terminal.name)
        } else {
            (terminal_id.to_string(), "Terminal".to_string())
        };

        let window = open_window(&directory, app_state, cx)?;
        window.update(cx, |multi_workspace, window, cx| {
            multi_workspace.workspace().update(cx, |workspace, cx| {
                PaseoTerminalItem::open(
                    workspace,
                    store.clone(),
                    terminal_id,
                    name.into(),
                    window,
                    cx,
                );
            });
        })?;
        wait(cx, Duration::from_secs(3))?;
        let path = output_dir.join("terminal-session.png");
        capture(cx, window, &path)?;
        println!("{}", path.display());
        print_terminal(window, cx)?;

        let command = std::env::var("PAZEDO_SCREENSHOT_TYPE")
            .unwrap_or_else(|_| "echo pazedo-$((6*7))".to_string());
        cx.simulate_input(window.into(), &command);
        cx.simulate_keystrokes(window.into(), "enter");
        wait(cx, Duration::from_secs(3))?;
        let path = output_dir.join("terminal-typed.png");
        capture(cx, window, &path)?;
        println!("{}", path.display());
        print_terminal(window, cx)
    }

    /// Clicks the points in `PAZEDO_SCREENSHOT_CLICK`: `x,y` in window points (the PNGs are
    /// captured at that scale), several separated by `;`. Returns whether it was set.
    fn click_from_env(
        window: WindowHandle<MultiWorkspace>,
        cx: &mut VisualTestAppContext,
    ) -> Result<bool> {
        let Ok(clicks) = std::env::var("PAZEDO_SCREENSHOT_CLICK") else {
            return Ok(false);
        };
        for click in clicks.split(';') {
            let (x, y) = click
                .split_once(',')
                .and_then(|(x, y)| {
                    Some((x.trim().parse::<f32>().ok()?, y.trim().parse::<f32>().ok()?))
                })
                .context("PAZEDO_SCREENSHOT_CLICK must be x,y[;x,y...]")?;
            // A real pointer hovers before it clicks; some controls only react once hovered.
            cx.simulate_mouse_move(
                window.into(),
                point(px(x), px(y)),
                None,
                gpui::Modifiers::none(),
            );
            cx.run_until_parked();
            cx.simulate_click(window.into(), point(px(x), px(y)), gpui::Modifiers::none());
            wait(cx, Duration::from_secs(2))?;
        }
        Ok(true)
    }

    fn print_terminal(
        window: WindowHandle<MultiWorkspace>,
        cx: &mut VisualTestAppContext,
    ) -> Result<()> {
        window.update(cx, |multi_workspace, _, cx| {
            let workspace = multi_workspace.workspace().read(cx);
            for item in workspace.items_of_type::<PaseoTerminalItem>(cx) {
                let content = item.read(cx).terminal().read(cx).get_content();
                println!("terminal content:\n{}\n---", content.trim_end());
            }
        })
    }

    fn live_agent_session(
        endpoint: &str,
        agent_id: &str,
        app_state: &Arc<AppState>,
        cx: &mut VisualTestAppContext,
        path: &std::path::Path,
    ) -> Result<()> {
        let store = cx.update(|cx| {
            let store = cx.new(|cx| PaseoStore::new(DaemonConfig::local(endpoint), cx));
            PaseoDaemons::set_global_for_test(vec![store.clone()], cx);
            store
        });
        // `new` starts a new agent in the sandbox, as the sidebar's + button does.
        let new_agent = agent_id == "new";
        let (directory, title) = if new_agent {
            pump(cx, Duration::from_secs(15), |cx| {
                store.read_with(cx, |store, _| {
                    store.client().is_some_and(|client| client.is_connected())
                })
            })
            .context("could not connect to the test daemon")?;
            (
                dirs_home().join("dev/pazedo-sandbox"),
                "New Agent".to_string(),
            )
        } else {
            pump(cx, Duration::from_secs(15), |cx| {
                store.read_with(cx, |store, _| store.agent(agent_id).is_some())
            })
            .context("the test daemon did not list the agent")?;
            store.read_with(cx, |store, _| {
                let agent = store.agent(agent_id).expect("checked above");
                (
                    PathBuf::from(&agent.cwd),
                    agent.title.clone().unwrap_or_default(),
                )
            })
        };

        let window = open_window(&directory, app_state, cx)?;
        window.update(cx, |multi_workspace, window, cx| {
            multi_workspace.workspace().update(cx, |workspace, cx| {
                PaseoAgentItem::open(
                    workspace,
                    store.clone(),
                    (!new_agent).then(|| agent_id.to_string()),
                    None,
                    title.into(),
                    window,
                    cx,
                );
            });
        })?;
        // History and live state arrive over the network; give the view time to fill.
        wait(cx, Duration::from_secs(8))?;
        capture(cx, window, path)?;
        dump_thread(window, cx)?;

        // Typing into the message editor and pressing enter sends through Paseo, like a user.
        if let Ok(message) = std::env::var("PAZEDO_SCREENSHOT_SEND") {
            cx.simulate_input(window.into(), &message);
            cx.simulate_keystrokes(window.into(), "enter");
            wait(cx, Duration::from_secs(3))?;
            capture(cx, window, &path.with_file_name("agent-session-sent.png"))?;
            println!(
                "{}",
                path.with_file_name("agent-session-sent.png").display()
            );
            dump_thread(window, cx)?;
        }
        // For example a permission button.
        if click_from_env(window, cx)? {
            wait(cx, Duration::from_secs(10))?;
            capture(
                cx,
                window,
                &path.with_file_name("agent-session-clicked.png"),
            )?;
            println!(
                "{}",
                path.with_file_name("agent-session-clicked.png").display()
            );
            dump_thread(window, cx)?;
        }
        // A message from another client while the tab is open, like `paseo send` from the CLI.
        if let Ok(message) = std::env::var("PAZEDO_SCREENSHOT_EXTERNAL_SEND") {
            let client = store
                .read_with(cx, |store, _| store.client())
                .context("not connected")?;
            let agent_id = agent_id.to_string();
            let send = cx.background_executor.spawn(async move {
                client
                    .request(&paseo_client::protocol::SendAgentMessage {
                        agent_id,
                        text: message,
                        message_id: None,
                        active_turn_behavior: None,
                        images: Vec::new(),
                    })
                    .await
            });
            pump(cx, Duration::from_secs(10), |_| send.is_ready())?;
            wait(cx, Duration::from_secs(4))?;
            capture(
                cx,
                window,
                &path.with_file_name("agent-session-external.png"),
            )?;
            println!(
                "{}",
                path.with_file_name("agent-session-external.png").display()
            );
            dump_thread(window, cx)?;
        }
        Ok(())
    }

    /// Prints the thread's entries and how many rows the view's list has for them.
    fn dump_thread(
        window: WindowHandle<MultiWorkspace>,
        cx: &mut VisualTestAppContext,
    ) -> Result<()> {
        window.update(cx, |multi_workspace, _, cx| {
            let workspace = multi_workspace.workspace().read(cx);
            for item in workspace.items_of_type::<PaseoAgentItem>(cx) {
                let Some(thread_view) = item
                    .read(cx)
                    .conversation_view()
                    .and_then(|view| view.read(cx).active_thread())
                else {
                    continue;
                };
                let thread_view = thread_view.read(cx);
                let thread = thread_view.thread.read(cx);
                println!(
                    "thread entries: {}, view list items: {}",
                    thread.entries().len(),
                    thread_view.list_state.item_count()
                );
                if std::env::var("PAZEDO_SCREENSHOT_DUMP_ENTRIES").is_ok() {
                    let view_state = thread_view.entry_view_state.read(cx);
                    for (index, entry) in thread.entries().iter().enumerate() {
                        let kind = format!("{entry:?}");
                        let kind = kind.split('(').next().unwrap_or_default().to_string();
                        let user_text = entry
                            .user_message()
                            .map(|message| message.content.to_markdown(cx));
                        let view_editor = view_state
                            .entry(index)
                            .and_then(|entry| entry.message_editor())
                            .map(|editor| editor.read(cx).text(cx));
                        println!(
                            "  {index}: {kind} user={user_text:?} view_editor={view_editor:?}"
                        );
                        let debug = format!("{entry:?}");
                        if debug.contains(":edit:") {
                            println!("    {}", debug.chars().take(1500).collect::<String>());
                        }
                    }
                }
            }
        })?;
        Ok(())
    }

    fn dirs_home() -> PathBuf {
        std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
    }

    fn wait(cx: &mut VisualTestAppContext, duration: Duration) -> Result<()> {
        let deadline = Instant::now() + duration;
        pump(cx, duration + Duration::from_secs(2), |_| {
            Instant::now() > deadline
        })
    }

    fn capture(
        cx: &mut VisualTestAppContext,
        window: WindowHandle<MultiWorkspace>,
        path: &std::path::Path,
    ) -> Result<()> {
        cx.update_window(window.into(), |_, window, _| window.refresh())?;
        cx.run_until_parked();
        cx.capture_screenshot(window.into())?.save(path)?;
        Ok(())
    }

    /// Runs the deterministic scheduler while real I/O (the daemon socket) makes progress.
    fn pump(
        cx: &mut VisualTestAppContext,
        timeout: Duration,
        mut done: impl FnMut(&mut VisualTestAppContext) -> bool,
    ) -> Result<()> {
        cx.background_executor.allow_parking();
        let start = Instant::now();
        let result = loop {
            if done(cx) {
                break Ok(());
            }
            if start.elapsed() > timeout {
                break Err(anyhow::anyhow!("timed out after {timeout:?}"));
            }
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(20));
            cx.advance_clock(Duration::from_millis(20));
        };
        cx.background_executor.forbid_parking();
        result
    }

    fn connected() -> ConnectionStatus {
        ConnectionStatus::Connected(Arc::new(
            serde_json::from_value::<ServerInfo>(json!({
                "status": "server_info",
                "serverId": "srv_fixture",
                "hostname": "My Mac",
                "version": "0.15.4",
            }))
            .expect("valid server info"),
        ))
    }

    fn open_window(
        project_dir: &std::path::Path,
        app_state: &Arc<AppState>,
        cx: &mut VisualTestAppContext,
    ) -> Result<WindowHandle<MultiWorkspace>> {
        let project = cx.update(|cx| {
            Project::local(
                app_state.client.clone(),
                app_state.node_runtime.clone(),
                app_state.user_store.clone(),
                app_state.languages.clone(),
                app_state.fs.clone(),
                None,
                project::LocalProjectFlags {
                    init_worktree_trust: false,
                    ..Default::default()
                },
                cx,
            )
        });
        let add_worktree = project.update(cx, |project, cx| {
            project.find_or_create_worktree(project_dir, true, cx)
        });
        cx.background_executor.allow_parking();
        cx.foreground_executor()
            .block_test(add_worktree)
            .context("failed to add the project worktree")?;
        cx.background_executor.forbid_parking();

        let window = cx.update(|cx| {
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: point(px(0.), px(0.)),
                        size: size(px(1100.), px(720.)),
                    })),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                |window, cx| {
                    let workspace = cx.new(|cx| {
                        workspace::Workspace::new(
                            None,
                            project.clone(),
                            app_state.clone(),
                            window,
                            cx,
                        )
                    });
                    cx.new(|cx| MultiWorkspace::new(workspace, window, cx))
                },
            )
        })?;
        cx.run_until_parked();

        // Created outside the MultiWorkspace update: the sidebar reads the MultiWorkspace.
        let sidebar = cx.update_window(window.into(), |root, window, cx| {
            let multi_workspace: Entity<MultiWorkspace> = root
                .downcast()
                .map_err(|_| anyhow::anyhow!("unexpected root view"))?;
            anyhow::Ok(cx.new(|cx| PaseoSidebar::new(multi_workspace, window, cx)))
        })??;
        window.update(cx, |multi_workspace, _, cx| {
            multi_workspace.register_sidebar(sidebar, cx);
            multi_workspace.open_sidebar(cx);
        })?;

        for _ in 0..10 {
            cx.advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
        }
        cx.update_window(window.into(), |_, window, _| window.refresh())?;
        cx.run_until_parked();
        Ok(window)
    }

    fn init_app_state(cx: &mut gpui::App) -> Arc<AppState> {
        let fs: Arc<dyn fs::Fs> = fs::RealFs::new(None, cx.background_executor().clone());
        <dyn fs::Fs>::set_global(fs.clone(), cx);
        let languages = Arc::new(language::LanguageRegistry::test(
            cx.background_executor().clone(),
        ));
        let clock = Arc::new(clock::FakeSystemClock::new());
        let http_client = http_client::FakeHttpClient::with_404_response();
        let client = client::Client::new(clock, http_client, cx);
        let session = cx.new(|cx| session::AppSession::new(session::Session::test(), cx));
        let user_store = cx.new(|cx| client::UserStore::new(client.clone(), cx));
        let workspace_store = cx.new(|cx| workspace::WorkspaceStore::new(client.clone(), cx));
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        client::init(&client, cx);
        let app_state = Arc::new(AppState {
            client,
            fs,
            languages,
            user_store,
            workspace_store,
            node_runtime: node_runtime::NodeRuntime::unavailable(),
            build_window_options: |_, _| Default::default(),
            session,
        });
        AppState::set_global(app_state.clone(), cx);
        app_state
    }

    fn ago(minutes: i64) -> String {
        (Utc::now() - ChronoDuration::minutes(minutes)).to_rfc3339()
    }

    fn project(id: &str, name: &str, root: &str) -> ProjectSummary {
        serde_json::from_value(json!({
            "projectId": id,
            "projectDisplayName": name,
            "projectRootPath": root,
            "projectKind": "git",
        }))
        .expect("valid project")
    }

    fn workspace(id: &str, project_id: &str, directory: &str, extra: Value) -> Workspace {
        let mut value = json!({
            "id": id,
            "projectId": project_id,
            "projectDisplayName": project_id,
            "projectRootPath": directory,
            "workspaceDirectory": directory,
            "projectKind": "git",
            "workspaceKind": "local_checkout",
            "name": "main",
            "status": "done",
            "activityAt": ago(3),
            "createdAt": ago(300),
            "gitRuntime": {"currentBranch": "main"},
        });
        merge(&mut value, extra);
        serde_json::from_value(value).expect("valid workspace")
    }

    fn agent(id: &str, workspace_id: &str, title: &str, extra: Value) -> Agent {
        let mut value = json!({
            "id": id,
            "provider": "claude",
            "cwd": "/",
            "workspaceId": workspace_id,
            "model": null,
            "createdAt": ago(120),
            "updatedAt": ago(4),
            "lastUserMessageAt": null,
            "status": "idle",
            "capabilities": {},
            "currentModeId": null,
            "availableModes": [],
            "pendingPermissions": [],
            "persistence": null,
            "title": title,
        });
        merge(&mut value, extra);
        serde_json::from_value(value).expect("valid agent")
    }

    fn merge(value: &mut Value, extra: Value) {
        if let (Value::Object(value), Value::Object(extra)) = (value, extra) {
            value.extend(extra);
        }
    }

    /// One project per state the sidebar distinguishes. `open_directory` is the window's
    /// project, so the first pazedo workspace is highlighted.
    fn fixture(open_directory: &str) -> (Vec<ProjectSummary>, Vec<Workspace>, Vec<Agent>) {
        let projects = vec![
            project("pazedo", "pazedo", open_directory),
            project("website", "website", "/Users/me/dev/website"),
            project(
                "sandbox",
                "pazedo-sandbox",
                "/Users/me/dev/pazedo-sandbox",
            ),
            project("empty", "notes", "/Users/me/notes"),
        ];
        let workspaces = vec![
            workspace(
                "wks_pazedo_main",
                "pazedo",
                open_directory,
                json!({"status": "running", "activityAt": ago(0), "diffStat": {"additions": 412, "deletions": 37}}),
            ),
            workspace(
                "wks_pazedo_terminals",
                "pazedo",
                "/Users/me/dev/pazedo-worktrees/terminals",
                json!({
                    "status": "needs_input",
                    "workspaceKind": "worktree",
                    "name": "Paseo terminals",
                    "title": "Paseo terminals",
                    "worktreeSlug": "terminals",
                    "gitRuntime": {"currentBranch": "pazedo/terminals"},
                    "githubRuntime": {"pullRequest": {
                        "number": 42, "url": "https://github.com/example/pazedo/pull/42",
                        "title": "Paseo terminals", "state": "OPEN", "baseRefName": "main",
                        "headRefName": "pazedo/terminals", "isMerged": false, "isDraft": false,
                        "checksStatus": "success", "reviewDecision": "approved",
                        "checks": [
                            {"name": "test", "status": "success", "url": null, "duration": "1m 17s"},
                            {"name": "clippy", "status": "success", "url": null, "duration": "3m 2s"},
                        ],
                    }},
                    "createdAt": ago(90),
                    "activityAt": ago(1),
                }),
            ),
            workspace(
                "wks_website_main",
                "website",
                "/Users/me/dev/website",
                json!({"status": "attention", "activityAt": ago(12)}),
            ),
            workspace(
                "wks_website_flaky",
                "website",
                "/Users/me/dev/website-worktrees/fix-flaky-board-test",
                json!({
                    "status": "failed",
                    "workspaceKind": "worktree",
                    "name": "fix-flaky-board-test-with-a-really-long-branch-name",
                    "worktreeSlug": "fix-flaky-board-test",
                    "gitRuntime": {"currentBranch": "fix-flaky-board-test-with-a-really-long-branch-name"},
                    "createdAt": ago(200),
                    "activityAt": ago(45),
                }),
            ),
            workspace(
                "wks_sandbox_1",
                "sandbox",
                "/Users/me/dev/pazedo-sandbox",
                json!({"activityAt": ago(60 * 26), "createdAt": ago(60 * 30)}),
            ),
            workspace(
                "wks_sandbox_2",
                "sandbox",
                "/Users/me/dev/pazedo-sandbox",
                json!({"activityAt": ago(60 * 3), "createdAt": ago(60 * 4)}),
            ),
        ];
        let agents = vec![
            agent(
                "a1",
                "wks_pazedo_main",
                "Implement the Paseo workspace sidebar",
                json!({"status": "running", "createdAt": ago(30), "updatedAt": ago(0)}),
            ),
            agent(
                "a2",
                "wks_pazedo_main",
                "Review protocol notes",
                json!({"provider": "codex", "createdAt": ago(100), "updatedAt": ago(80)}),
            ),
            agent(
                "a3",
                "wks_pazedo_terminals",
                "Stream Paseo terminals into Zed's terminal view",
                json!({
                    "status": "running",
                    "pendingPermissions": [{"id": "perm", "provider": "claude", "name": "Bash", "kind": "tool", "title": "cargo build -p zed"}],
                    "attentionReason": "permission",
                    "updatedAt": ago(1),
                }),
            ),
            agent(
                "a4",
                "wks_website_main",
                "Dispatch ISSUE-42 to a worker",
                json!({"requiresAttention": true, "attentionReason": "finished", "updatedAt": ago(12)}),
            ),
            agent(
                "a5",
                "wks_website_main",
                "",
                json!({"provider": "opencode", "status": "running", "createdAt": ago(5), "updatedAt": ago(0)}),
            ),
            agent(
                "a6",
                "wks_website_flaky",
                "Fix the flaky board drag-and-drop test",
                json!({"status": "error", "lastError": "Provider exited with code 1", "updatedAt": ago(45)}),
            ),
            agent(
                "a7",
                "wks_sandbox_2",
                "mock smoke",
                json!({"provider": "mock", "status": "closed", "updatedAt": ago(60 * 3)}),
            ),
        ];
        (projects, workspaces, agents)
    }
}
