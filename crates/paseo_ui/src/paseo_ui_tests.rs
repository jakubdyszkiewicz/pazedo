use crate::{
    DaemonConfig, PaseoAgentItem, PaseoDaemons, PaseoPanel, PaseoSidebar, PaseoStore,
    PaseoTerminalItem,
    store::{DirectorySnapshot, DirectorySubscriptions},
};
use fs::FakeFs;
use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use paseo_client::{
    Event,
    frames::{TerminalFrame, TerminalOpcode},
    protocol::{
        Agent, AgentUpdate, ProjectSummary, ServerMessage, TerminalInfo, TerminalsChanged,
        Workspace, WorkspaceStatus, WorkspaceUpdate,
    },
};
use pretty_assertions::assert_eq;
use project::Project;
use serde_json::{Value, json};
use settings::SettingsStore;
use std::path::Path;
use util::path;
use workspace::{MultiWorkspace, Sidebar as _};

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
    });
}

fn workspace(id: &str, project_id: &str, directory: &str, created_at: &str) -> Workspace {
    serde_json::from_value(json!({
        "id": id,
        "projectId": project_id,
        "projectDisplayName": project_id.trim_start_matches("prj_"),
        "projectRootPath": directory,
        "workspaceDirectory": directory,
        "projectKind": "git",
        "workspaceKind": "local_checkout",
        "name": "main",
        "status": "done",
        "activityAt": null,
        "createdAt": created_at,
    }))
    .unwrap()
}

fn with_status(mut value: Workspace, status: WorkspaceStatus) -> Workspace {
    value.status = status;
    value
}

fn agent(id: &str, workspace_id: &str, status: &str, extra: Value) -> Agent {
    let mut agent = json!({
        "id": id,
        "provider": "mock",
        "cwd": "/sandbox",
        "workspaceId": workspace_id,
        "model": null,
        "createdAt": format!("2026-11-14T10:00:0{}Z", id.len() % 10),
        "updatedAt": "2026-11-14T10:00:00Z",
        "lastUserMessageAt": null,
        "status": status,
        "capabilities": {},
        "currentModeId": null,
        "availableModes": [],
        "pendingPermissions": [],
        "persistence": null,
        "title": id,
    });
    if let (Value::Object(agent), Value::Object(extra)) = (&mut agent, extra) {
        agent.extend(extra);
    }
    serde_json::from_value(agent).unwrap()
}

fn project(id: &str, name: &str, root: &str) -> ProjectSummary {
    serde_json::from_value(json!({
        "projectId": id,
        "projectDisplayName": name,
        "projectRootPath": root,
        "projectKind": "git",
    }))
    .unwrap()
}

fn workspace_upsert(subscription: &str, workspace: Workspace) -> Event {
    Event::Message(ServerMessage::WorkspaceUpdate(WorkspaceUpdate::Upsert {
        subscription_id: Some(subscription.into()),
        workspace,
    }))
}

fn snapshot(
    projects: Vec<ProjectSummary>,
    workspaces: Vec<Workspace>,
    agents: Vec<Agent>,
) -> DirectorySnapshot {
    DirectorySnapshot {
        projects,
        workspaces,
        agents,
        subscriptions: DirectorySubscriptions {
            workspaces: Some("workspaces-2".into()),
            agents: Some("agents-2".into()),
        },
    }
}

fn new_store(cx: &mut TestAppContext) -> Entity<PaseoStore> {
    cx.new(|cx| PaseoStore::new(DaemonConfig::default(), cx))
}

fn workspace_names(store: &Entity<PaseoStore>, cx: &TestAppContext) -> Vec<(String, Vec<String>)> {
    store.read_with(cx, |store, _| {
        store
            .project_groups()
            .into_iter()
            .map(|group| {
                (
                    group.name.to_string(),
                    group
                        .workspaces
                        .into_iter()
                        .map(|entry| entry.workspace.id)
                        .collect(),
                )
            })
            .collect()
    })
}

#[gpui::test]
fn test_updates_during_sync_apply_after_the_snapshot(cx: &mut TestAppContext) {
    let store = new_store(cx);
    store.update(cx, |store, cx| {
        store.begin_sync();
        // Arrives on the new subscription before the snapshot response is applied.
        store.handle_event(
            workspace_upsert(
                "workspaces-2",
                with_status(
                    workspace("wks_a", "prj_a", "/a", "2026-11-14T10:00:00Z"),
                    WorkspaceStatus::Running,
                ),
            ),
            cx,
        );
        // A late push from the previous connection's subscription.
        store.handle_event(
            workspace_upsert(
                "workspaces-1",
                with_status(
                    workspace("wks_a", "prj_a", "/a", "2026-11-14T10:00:00Z"),
                    WorkspaceStatus::Failed,
                ),
            ),
            cx,
        );
        store.finish_sync(Ok(snapshot(
            vec![project("prj_a", "a", "/a")],
            vec![workspace("wks_a", "prj_a", "/a", "2026-11-14T10:00:00Z")],
            Vec::new(),
        )));
        assert_eq!(
            store.workspace("wks_a").map(|workspace| workspace.status),
            Some(WorkspaceStatus::Running)
        );

        store.handle_event(
            workspace_upsert(
                "workspaces-1",
                with_status(
                    workspace("wks_a", "prj_a", "/a", "2026-11-14T10:00:00Z"),
                    WorkspaceStatus::Failed,
                ),
            ),
            cx,
        );
        assert_eq!(
            store.workspace("wks_a").map(|workspace| workspace.status),
            Some(WorkspaceStatus::Running),
            "pushes from a released subscription are ignored"
        );
    });
}

#[gpui::test]
fn test_groups_workspaces_and_agents(cx: &mut TestAppContext) {
    let store = new_store(cx);
    store.update(cx, |store, cx| {
        store.begin_sync();
        store.finish_sync(Ok(snapshot(
            vec![
                project("prj_b", "beta", "/b"),
                project("prj_a", "Alpha", "/a"),
                project("prj_empty", "empty", "/empty"),
            ],
            vec![
                workspace("wks_old", "prj_a", "/a", "2026-11-14T08:00:00Z"),
                workspace("wks_new", "prj_a", "/a", "2026-11-14T09:00:00Z"),
                workspace("wks_b", "prj_b", "/b", "2026-11-14T09:00:00Z"),
                // Its project is not known yet; it still gets a group.
                workspace("wks_c", "prj_gamma", "/c", "2026-11-14T09:00:00Z"),
            ],
            vec![
                agent("one", "wks_new", "running", json!({})),
                agent(
                    "two",
                    "wks_new",
                    "idle",
                    json!({"pendingPermissions": [
                        {"id": "p", "provider": "mock", "name": "bash", "kind": "tool"}
                    ]}),
                ),
                agent("three", "wks_old", "idle", json!({})),
            ],
        )));

        store.handle_event(
            Event::Message(ServerMessage::AgentUpdate(AgentUpdate::Upsert {
                subscription_id: Some("agents-2".into()),
                agent: agent("four", "wks_b", "initializing", json!({})),
                project: None,
            })),
            cx,
        );
        store.handle_event(
            Event::Message(ServerMessage::AgentUpdate(AgentUpdate::Remove {
                subscription_id: Some("agents-2".into()),
                agent_id: "three".into(),
            })),
            cx,
        );

        let groups = store.project_groups();
        let new_workspace = &groups[0].workspaces[0];
        assert_eq!(new_workspace.workspace.id, "wks_new");
        assert_eq!(new_workspace.agents.len(), 2);
        assert_eq!(new_workspace.running_agent_count(), 1);
        assert_eq!(new_workspace.pending_permission_count(), 1);
        assert!(groups[0].workspaces[1].agents.is_empty());
        assert_eq!(groups[1].workspaces[0].running_agent_count(), 1);
    });

    assert_eq!(
        workspace_names(&store, cx),
        vec![
            (
                "Alpha".to_string(),
                vec!["wks_new".to_string(), "wks_old".to_string()]
            ),
            ("beta".to_string(), vec!["wks_b".to_string()]),
            ("empty".to_string(), Vec::new()),
            ("gamma".to_string(), vec!["wks_c".to_string()]),
        ]
    );
}

#[gpui::test]
fn test_workspace_removal_updates_projects(cx: &mut TestAppContext) {
    let store = new_store(cx);
    store.update(cx, |store, cx| {
        store.begin_sync();
        store.finish_sync(Ok(snapshot(
            vec![project("prj_a", "a", "/a"), project("prj_b", "b", "/b")],
            vec![
                workspace("wks_a", "prj_a", "/a", "2026-11-14T10:00:00Z"),
                workspace("wks_b", "prj_b", "/b", "2026-11-14T10:00:00Z"),
            ],
            Vec::new(),
        )));
        store.handle_event(
            Event::Message(ServerMessage::WorkspaceUpdate(WorkspaceUpdate::Remove {
                subscription_id: Some("workspaces-2".into()),
                id: "wks_b".into(),
                empty_project: None,
                removed_project_id: Some("prj_b".into()),
            })),
            cx,
        );
        store.handle_event(
            Event::Message(ServerMessage::WorkspaceUpdate(WorkspaceUpdate::Remove {
                subscription_id: Some("workspaces-2".into()),
                id: "wks_a".into(),
                empty_project: Some(project("prj_a", "a renamed", "/a")),
                removed_project_id: None,
            })),
            cx,
        );
    });
    assert_eq!(
        workspace_names(&store, cx),
        vec![("a renamed".to_string(), Vec::new())]
    );
}

#[gpui::test]
fn test_disconnect_keeps_the_last_directory(cx: &mut TestAppContext) {
    let store = new_store(cx);
    store.update(cx, |store, cx| {
        store.begin_sync();
        store.finish_sync(Ok(snapshot(
            vec![project("prj_a", "a", "/a")],
            vec![workspace("wks_a", "prj_a", "/a", "2026-11-14T10:00:00Z")],
            Vec::new(),
        )));
        store.handle_event(
            Event::Disconnected {
                reason: "connection reset".into(),
            },
            cx,
        );
        assert!(matches!(
            store.status(),
            crate::ConnectionStatus::Disconnected(reason) if reason.as_ref() == "connection reset"
        ));
        assert!(store.workspace("wks_a").is_some());
    });
}

#[gpui::test]
async fn test_selecting_a_workspace_activates_its_directory(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/sandbox"), json!({ "main.rs": "" }))
        .await;
    fs.insert_tree(path!("/other"), json!({ "lib.rs": "" }))
        .await;
    let other_project = Project::test(fs.clone(), [path!("/other").as_ref()], cx).await;
    let sandbox_project = Project::test(fs, [path!("/sandbox").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(other_project, window, cx));
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(sandbox_project, window, cx);
    });
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        let other = multi_workspace
            .workspaces()
            .find(|workspace| root_is(workspace, path!("/other"), cx))
            .cloned()
            .unwrap();
        multi_workspace.activate(other, None, window, cx);
    });
    cx.run_until_parked();

    let (sidebar, store) = sidebar_with_directory(&multi_workspace, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_workspace(&store, "wks_second", window, cx);
    });
    cx.run_until_parked();

    multi_workspace.read_with(cx, |multi_workspace, cx| {
        assert!(root_is(multi_workspace.workspace(), path!("/sandbox"), cx));
    });
    sidebar.read_with(cx, |sidebar, cx| {
        assert_eq!(
            sidebar.highlighted_workspace_id(cx).as_deref(),
            Some("wks_second"),
            "the clicked row is highlighted even though another workspace shares its directory"
        );
    });
}

fn sidebar_with_directory(
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut VisualTestContext,
) -> (Entity<PaseoSidebar>, Entity<PaseoStore>) {
    let store = cx.update(|_, cx| {
        let store = cx.new(|cx| PaseoStore::new(DaemonConfig::default(), cx));
        PaseoDaemons::set_global_for_test(vec![store.clone()], cx);
        store
    });
    let sidebar =
        cx.update(|window, cx| cx.new(|cx| PaseoSidebar::new(multi_workspace.clone(), window, cx)));
    sidebar.update(cx, |sidebar, cx| {
        store.update(cx, |store, _| {
            store.begin_sync();
            store.finish_sync(Ok(snapshot(
                vec![project("prj_sandbox", "sandbox", path!("/sandbox"))],
                vec![
                    workspace(
                        "wks_first",
                        "prj_sandbox",
                        path!("/sandbox"),
                        "2026-11-14T09:00:00Z",
                    ),
                    workspace(
                        "wks_second",
                        "prj_sandbox",
                        path!("/sandbox"),
                        "2026-11-14T10:00:00Z",
                    ),
                ],
                Vec::new(),
            )));
        });
        assert!(!sidebar.has_notifications(cx));
    });
    (sidebar, store)
}

fn root_is(workspace: &Entity<workspace::Workspace>, path: &str, cx: &gpui::App) -> bool {
    workspace.read(cx).root_paths(cx).as_slice() == [Path::new(path).into()]
}

fn terminals_changed(workspace_id: &str, subscription: &str, names: &[&str]) -> Event {
    Event::Message(ServerMessage::TerminalsChanged(TerminalsChanged {
        subscription_id: Some(subscription.into()),
        workspace_id: Some(workspace_id.into()),
        cwd: path!("/sandbox").into(),
        terminals: names
            .iter()
            .map(|name| TerminalInfo {
                id: format!("term-{name}"),
                name: name.to_string(),
                cwd: Some(path!("/sandbox").into()),
                workspace_id: Some(workspace_id.into()),
                title: None,
            })
            .collect(),
    }))
}

fn terminal_names(
    store: &Entity<PaseoStore>,
    workspace_id: &str,
    cx: &TestAppContext,
) -> Vec<String> {
    store.read_with(cx, |store, _| {
        store
            .project_groups()
            .into_iter()
            .flat_map(|group| group.workspaces)
            .filter(|entry| entry.workspace.id == workspace_id)
            .flat_map(|entry| entry.terminals)
            .map(|terminal| terminal.name)
            .collect()
    })
}

#[gpui::test]
fn test_terminal_lists_follow_watched_workspaces(cx: &mut TestAppContext) {
    init_test(cx);
    let store = new_store(cx);
    store.update(cx, |store, cx| {
        store.begin_sync();
        store.finish_sync(Ok(snapshot(
            vec![project("prj_sandbox", "sandbox", path!("/sandbox"))],
            vec![
                workspace(
                    "wks_watched",
                    "prj_sandbox",
                    path!("/sandbox"),
                    "2026-11-14T09:00:00Z",
                ),
                workspace(
                    "wks_other",
                    "prj_sandbox",
                    path!("/sandbox"),
                    "2026-11-14T10:00:00Z",
                ),
            ],
            Vec::new(),
        )));
        store.watch_terminals("wks_watched", cx);
        store.handle_event(
            terminals_changed("wks_watched", "terminals-1", &["zsh", "server"]),
            cx,
        );
        store.handle_event(
            terminals_changed("wks_other", "terminals-2", &["ignored"]),
            cx,
        );
    });
    assert_eq!(
        terminal_names(&store, "wks_watched", cx),
        vec!["zsh", "server"]
    );
    assert_eq!(
        terminal_names(&store, "wks_other", cx),
        Vec::<String>::new()
    );

    store.update(cx, |store, cx| {
        store.handle_event(
            terminals_changed("wks_watched", "terminals-1", &["zsh"]),
            cx,
        );
    });
    assert_eq!(
        terminal_names(&store, "wks_watched", cx),
        vec!["zsh"],
        "later pushes carry the full list"
    );
}

#[gpui::test]
async fn test_terminal_tab_shows_the_restored_screen_then_output(cx: &mut TestAppContext) {
    init_test(cx);
    let store = cx.update(|cx| {
        let store = cx.new(|cx| PaseoStore::new(DaemonConfig::default(), cx));
        PaseoDaemons::set_global_for_test(vec![store.clone()], cx);
        store
    });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/sandbox"), json!({ "main.rs": "" }))
        .await;
    let project = Project::test(fs, [path!("/sandbox").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    workspace.update_in(cx, |workspace, window, cx| {
        PaseoTerminalItem::open(
            workspace,
            store.clone(),
            "term-1".into(),
            "zsh".into(),
            window,
            cx,
        );
        PaseoTerminalItem::open(
            workspace,
            store.clone(),
            "term-1".into(),
            "zsh".into(),
            window,
            cx,
        );
    });
    let items: Vec<Entity<PaseoTerminalItem>> = workspace.read_with(cx, |workspace, cx| {
        workspace.items_of_type::<PaseoTerminalItem>(cx).collect()
    });
    assert_eq!(items.len(), 1, "opening an open terminal focuses its tab");
    let item = items[0].clone();

    item.update(cx, |item, cx| {
        for (opcode, payload) in [
            (TerminalOpcode::Output, &b"stale output"[..]),
            (TerminalOpcode::Restore, b"$ echo hi\r\nhi\r\n$ "),
            (TerminalOpcode::Output, b"ls"),
        ] {
            item.apply_frame(
                TerminalFrame {
                    opcode,
                    slot: 0,
                    payload: payload.to_vec(),
                },
                cx,
            );
        }
    });
    cx.run_until_parked();
    item.read_with(cx, |item, cx| {
        let content = item.terminal().read(cx).get_content();
        assert_eq!(content.trim_end(), "$ echo hi\nhi\n$ ls");
    });
}

#[gpui::test]
async fn test_paseo_tabs_open_in_the_paseo_panel(cx: &mut TestAppContext) {
    init_test(cx);
    let store = cx.update(|cx| {
        let store = cx.new(|cx| PaseoStore::new(DaemonConfig::default(), cx));
        PaseoDaemons::set_global_for_test(vec![store.clone()], cx);
        store
    });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/sandbox"), json!({ "main.rs": "" }))
        .await;
    let project = Project::test(fs, [path!("/sandbox").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let panel = workspace.update_in(cx, |workspace, window, cx| {
        let weak_workspace = workspace.weak_handle();
        let project = workspace.project().clone();
        let panel = cx.new(|cx| PaseoPanel::new(weak_workspace, project, window, cx));
        workspace.add_panel(panel.clone(), window, cx);
        panel
    });

    workspace.update_in(cx, |workspace, window, cx| {
        PaseoTerminalItem::open(
            workspace,
            store.clone(),
            "term-1".into(),
            "zsh".into(),
            window,
            cx,
        );
        PaseoTerminalItem::open(
            workspace,
            store.clone(),
            "term-2".into(),
            "server".into(),
            window,
            cx,
        );
        PaseoTerminalItem::open(
            workspace,
            store.clone(),
            "term-1".into(),
            "zsh".into(),
            window,
            cx,
        );
    });
    cx.run_until_parked();

    let pane = panel.read_with(cx, |panel, _| panel.pane().clone());
    pane.read_with(cx, |pane, cx| {
        assert_eq!(pane.items_len(), 2, "reopening a tab activates it");
        let active = pane
            .active_item()
            .and_then(|item| item.downcast::<PaseoTerminalItem>())
            .expect("a Paseo terminal is active");
        assert_eq!(active.read(cx).terminal_id(), "term-1");
    });
    workspace.read_with(cx, |workspace, cx| {
        assert_eq!(
            workspace.items_of_type::<PaseoTerminalItem>(cx).count(),
            0,
            "nothing opens in the center pane"
        );
        assert!(workspace.panel::<PaseoPanel>(cx).is_some());
    });
}

/// The ids of the panel's tabs, agents first as they are added.
fn panel_tabs(panel: &Entity<PaseoPanel>, cx: &mut VisualTestContext) -> Vec<String> {
    panel.read_with(cx, |panel, cx| {
        panel
            .pane()
            .read(cx)
            .items()
            .filter_map(|item| {
                if let Some(agent) = item.downcast::<PaseoAgentItem>() {
                    agent.read(cx).agent_id(cx)
                } else {
                    item.downcast::<PaseoTerminalItem>()
                        .map(|terminal| terminal.read(cx).terminal_id().to_string())
                }
            })
            .collect()
    })
}

#[gpui::test]
async fn test_panel_tabs_follow_the_open_paseo_workspace(cx: &mut TestAppContext) {
    init_test(cx);
    let store = cx.update(|cx| {
        let store = cx.new(|cx| PaseoStore::new(DaemonConfig::default(), cx));
        PaseoDaemons::set_global_for_test(vec![store.clone()], cx);
        store
    });
    store.update(cx, |store, cx| {
        store.begin_sync();
        store.finish_sync(Ok(snapshot(
            vec![project("prj_sandbox", "sandbox", path!("/sandbox"))],
            vec![
                workspace(
                    "wks_old",
                    "prj_sandbox",
                    path!("/sandbox"),
                    "2026-11-14T09:00:00Z",
                ),
                workspace(
                    "wks_new",
                    "prj_sandbox",
                    path!("/sandbox"),
                    "2026-11-14T10:00:00Z",
                ),
            ],
            vec![
                agent("a1", "wks_new", "idle", json!({})),
                agent("a22", "wks_new", "running", json!({})),
                agent(
                    "archived",
                    "wks_new",
                    "idle",
                    json!({"archivedAt": "2026-11-14T11:00:00Z"}),
                ),
                agent("elsewhere", "wks_old", "idle", json!({})),
            ],
        )));
        store.set_terminals(
            "wks_new",
            vec![TerminalInfo {
                id: "t1".into(),
                name: "Terminal 1".into(),
                cwd: Some(path!("/sandbox").into()),
                workspace_id: Some("wks_new".into()),
                title: None,
            }],
            cx,
        );
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/sandbox"), json!({ "main.rs": "" }))
        .await;
    let project = Project::test(fs, [path!("/sandbox").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let panel = workspace.update_in(cx, |workspace, window, cx| {
        let weak_workspace = workspace.weak_handle();
        let project = workspace.project().clone();
        let panel = cx.new(|cx| PaseoPanel::new(weak_workspace, project, window, cx));
        workspace.add_panel(panel.clone(), window, cx);
        panel
    });
    cx.run_until_parked();
    assert_eq!(
        panel_tabs(&panel, cx),
        vec!["a1", "a22", "t1"],
        "the newest workspace of the folder, without archived agents"
    );

    // Opening the other workspace of the same folder from the sidebar swaps the tabs, without
    // closing (archiving) anything.
    store.update(cx, |store, cx| store.select_workspace("wks_old", cx));
    cx.run_until_parked();
    assert_eq!(panel_tabs(&panel, cx), vec!["elsewhere"]);
    store.read_with(cx, |store, _| {
        assert!(!store.is_closing("a1") && !store.is_closing("t1"));
    });

    // An agent started elsewhere in the open workspace gets a tab.
    store.update(cx, |store, cx| {
        store.handle_event(
            Event::Message(ServerMessage::AgentUpdate(AgentUpdate::Upsert {
                subscription_id: None,
                agent: agent("from-phone", "wks_old", "running", json!({})),
                project: None,
            })),
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(panel_tabs(&panel, cx), vec!["elsewhere", "from-phone"]);
}

#[gpui::test]
fn test_the_daemon_settings_connect_and_reconnect(cx: &mut TestAppContext) {
    if std::env::var(crate::ENDPOINT_ENV_VAR).is_ok() {
        // The environment variable replaces the settings.
        return;
    }
    init_test(cx);
    let set_paseo = |paseo: Option<settings::PaseoSettingsContent>, cx: &mut TestAppContext| {
        cx.update(|cx| {
            gpui::BorrowAppContext::update_global::<SettingsStore, _>(cx, |settings, cx| {
                settings.update_user_settings(cx, |content| content.paseo = paseo);
            });
        });
    };
    let single = |endpoint: &str| settings::PaseoSettingsContent {
        endpoint: Some(endpoint.to_string()),
        ..Default::default()
    };
    let daemons = cx.update(PaseoDaemons::global);
    let statuses = |cx: &mut TestAppContext| {
        daemons.read_with(cx, |daemons, cx| {
            daemons
                .stores()
                .iter()
                .map(|store| format!("{:?}", store.read(cx).status()))
                .collect::<Vec<_>>()
        })
    };
    let stores =
        |cx: &mut TestAppContext| daemons.read_with(cx, |daemons, _| daemons.stores().to_vec());
    assert!(stores(cx).is_empty());

    set_paseo(Some(single("127.0.0.1:1")), cx);
    assert!(
        statuses(cx)[0].contains("Connecting(Tcp"),
        "{:?}",
        statuses(cx)
    );
    let first = stores(cx)[0].clone();
    assert!(first.read_with(cx, |store, _| store.client().is_some()));

    set_paseo(
        Some(settings::PaseoSettingsContent {
            endpoint: Some("127.0.0.1:1".into()),
            name: None,
            ssh: None,
            daemons: Some(vec![settings::PaseoDaemonContent {
                name: Some("dev box".into()),
                endpoint: Some("127.0.0.1:2".into()),
                ssh: Some("root@devbox".into()),
            }]),
        }),
        cx,
    );
    let both = stores(cx);
    assert_eq!(both.len(), 2);
    assert_eq!(
        both[0], first,
        "a daemon whose endpoint stays keeps its store"
    );
    assert_eq!(
        both[1].read_with(cx, |store, _| store.display_name()),
        "dev box"
    );

    set_paseo(Some(single("ws://")), cx);
    assert_eq!(stores(cx).len(), 1);
    assert!(statuses(cx)[0].starts_with("InvalidEndpoint"));
    assert!(
        first.read_with(cx, |store, _| store.client().is_none()),
        "a removed daemon's store disconnects even if an open tab keeps it"
    );

    set_paseo(None, cx);
    assert!(stores(cx).is_empty());
}

#[gpui::test]
fn test_finished_and_failed_agents_are_unseen_until_cleared(cx: &mut TestAppContext) {
    let store = new_store(cx);
    store.update(cx, |store, _| {
        store.begin_sync();
        store.finish_sync(Ok(snapshot(
            vec![project("prj_sandbox", "sandbox", "/sandbox")],
            vec![with_status(
                workspace("wks", "prj_sandbox", "/sandbox", "2026-11-14T09:00:00Z"),
                WorkspaceStatus::Attention,
            )],
            vec![
                agent(
                    "finished",
                    "wks",
                    "idle",
                    json!({
                        "requiresAttention": true,
                        "attentionReason": "finished",
                        "attentionTimestamp": "2026-11-14T10:00:00Z",
                    }),
                ),
                agent(
                    "failed",
                    "wks",
                    "error",
                    json!({"requiresAttention": true, "attentionReason": "error"}),
                ),
                agent(
                    "asking",
                    "wks",
                    "idle",
                    json!({"requiresAttention": true, "attentionReason": "permission"}),
                ),
                agent("seen", "wks", "idle", json!({})),
            ],
        )));
        assert!(store.has_unseen_attention("finished"));
        assert!(store.has_unseen_attention("failed"));
        assert!(
            !store.has_unseen_attention("asking"),
            "a permission request stays until it is answered"
        );
        assert!(!store.has_unseen_attention("seen"));
        assert!(!store.has_unseen_attention("unknown"));
    });
}

#[gpui::test]
async fn test_the_ssh_setting_puts_the_daemon_on_a_remote_host(cx: &mut TestAppContext) {
    if std::env::var(crate::ENDPOINT_ENV_VAR).is_ok() {
        // The environment variables replace the settings.
        return;
    }
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/sandbox"), json!({ "main.rs": "" }))
        .await;
    let project = Project::test(fs, [path!("/sandbox").as_ref()], cx).await;
    cx.update(|cx| {
        gpui::BorrowAppContext::update_global::<SettingsStore, _>(cx, |settings, cx| {
            settings.update_user_settings(cx, |content| {
                content.paseo = Some(settings::PaseoSettingsContent {
                    endpoint: Some("127.0.0.1:1".into()),
                    name: None,
                    ssh: None,
                    daemons: Some(vec![
                        settings::PaseoDaemonContent {
                            name: None,
                            endpoint: Some("127.0.0.1:2".into()),
                            ssh: Some("root@devbox".into()),
                        },
                        settings::PaseoDaemonContent {
                            name: None,
                            endpoint: Some("127.0.0.1:3".into()),
                            ssh: Some("ssh -p".into()),
                        },
                    ]),
                });
                content.remote.ssh_connections = Some(vec![settings::SshConnection {
                    host: "devbox".into(),
                    username: Some("root".into()),
                    args: vec!["-A".into()],
                    nickname: Some("dev box".into()),
                    ..Default::default()
                }]);
            });
        });
    });
    // Stores from the parsed settings, but without endpoints: connecting is not part of this.
    let stores = cx.update(|cx| {
        let configs = <crate::store::PaseoSettings as settings::Settings>::get_global(cx)
            .daemons
            .clone();
        assert_eq!(
            configs
                .iter()
                .map(|config| config.endpoint.as_deref())
                .collect::<Vec<_>>(),
            vec![
                Some("127.0.0.1:1"),
                Some("127.0.0.1:2"),
                Some("127.0.0.1:3")
            ],
            "the top-level daemon comes first"
        );
        configs
            .into_iter()
            .map(|config| {
                cx.new(|cx| {
                    PaseoStore::new(
                        DaemonConfig {
                            endpoint: None,
                            ..config
                        },
                        cx,
                    )
                })
            })
            .collect::<Vec<_>>()
    });

    cx.update(|cx| {
        let [local, remote, invalid] = stores.as_slice() else {
            panic!("expected three daemons, got {}", stores.len());
        };
        assert_eq!(local.read(cx).host(cx).unwrap(), None);
        assert!(
            local.read(cx).is_on_host(project.read(cx), cx),
            "a local folder belongs to a daemon on this machine"
        );

        let Some(remote::RemoteConnectionOptions::Ssh(options)) = remote.read(cx).host(cx).unwrap()
        else {
            panic!("expected an SSH host");
        };
        assert_eq!(options.host.to_string(), "devbox");
        assert_eq!(options.username.as_deref(), Some("root"));
        assert_eq!(
            options.nickname.as_deref(),
            Some("dev box"),
            "the matching ssh_connections entry fills in its options"
        );
        assert_eq!(options.args, Some(vec!["-A".to_string()]));
        assert!(
            !remote.read(cx).is_on_host(project.read(cx), cx),
            "a local folder must not show a remote daemon's workspace of the same path"
        );

        assert!(invalid.read(cx).host(cx).is_err());
    });
}

#[test]
fn renaming_a_daemon_writes_its_settings_entry() {
    let mut content = settings::SettingsContent::default();
    content.paseo = Some(settings::PaseoSettingsContent {
        endpoint: Some("100.64.0.1:6767".into()),
        name: None,
        ssh: None,
        daemons: Some(vec![settings::PaseoDaemonContent {
            name: None,
            endpoint: Some("100.64.0.2:6767".into()),
            ssh: Some("root@dev-box".into()),
        }]),
    });

    assert!(crate::store::set_daemon_name(
        &mut content,
        "100.64.0.1:6767",
        Some("mini".into())
    ));
    assert!(crate::store::set_daemon_name(
        &mut content,
        "100.64.0.2:6767",
        Some("dev box".into())
    ));
    assert!(!crate::store::set_daemon_name(
        &mut content,
        "127.0.0.1:1",
        Some("nowhere".into())
    ));
    let paseo = content.paseo.as_ref().unwrap();
    assert_eq!(paseo.name.as_deref(), Some("mini"));
    assert_eq!(
        paseo.daemons.as_ref().unwrap()[0].name.as_deref(),
        Some("dev box")
    );

    assert!(crate::store::set_daemon_name(
        &mut content,
        "100.64.0.2:6767",
        None
    ));
    assert_eq!(content.paseo.unwrap().daemons.unwrap()[0].name, None);
}
