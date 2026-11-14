//! Renders the Paseo sidebar in a real Zed window, off screen, to PNG files.
//!
//!   cargo run -p paseo_ui --example sidebar_screenshots --features test-support
//!
//! Uses gpui's `VisualTestAppContext` like Zed's own visual test runner: real Metal rendering
//! into a texture, a deterministic scheduler, and no visible window, so it needs neither Screen
//! Recording permission nor an uncovered window. The store is fed scripted fixtures instead of
//! a daemon. Images go to `target/pazedo-screenshots/` (override with `PAZEDO_SCREENSHOT_DIR`).

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
    use paseo_ui::{ConnectionStatus, PaseoSidebar, PaseoStore};
    use project::Project;
    use serde_json::{Value, json};
    use std::{path::PathBuf, sync::Arc, time::Duration};
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
            editor::init(cx);
            call::init(app_state.client.clone(), app_state.user_store.clone(), cx);
            title_bar::init(cx);
        });
        cx.run_until_parked();

        let store = cx.update(|cx| {
            let store = cx.new(|cx| PaseoStore::new(None, cx));
            PaseoStore::set_global(store.clone(), cx);
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
        Ok(())
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
