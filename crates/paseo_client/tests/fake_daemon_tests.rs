use anyhow::Result;
use futures::{StreamExt as _, channel::mpsc};
use paseo_client::{
    Client, ConnectOptions, Event, HelloRejected, RpcError,
    fake_daemon::{FakeConnection, FakeDaemon, smol_sleep, with_timeout},
    frames::{TerminalFrame, TerminalOpcode},
    protocol::{
        FetchWorkspaces, HelloAuth, HelloRejectionReason, ListProjects, ListUsageReports,
        ServerMessage, Subscribe, SubscribeTerminal, TerminalClientMessage, TerminalInput,
        UsageProblem, UsageReport, UsageTone, WorkspaceStatus, WorkspaceUpdate,
    },
};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::time::Duration;

struct Harness {
    daemon: FakeDaemon,
    client: Client,
    events: mpsc::UnboundedReceiver<Event>,
    _driver: smol::Task<()>,
}

impl Harness {
    async fn start(configure: impl FnOnce(&mut ConnectOptions)) -> Result<Self> {
        let daemon = FakeDaemon::start().await?;
        let mut options = ConnectOptions::new(daemon.endpoint(), "test-client", smol_sleep());
        options.reconnect_base_delay = Duration::from_millis(10);
        options.reconnect_max_delay = Duration::from_millis(50);
        configure(&mut options);
        let (client, events, driver) = Client::new(options);
        Ok(Self {
            daemon,
            client,
            events,
            _driver: smol::spawn(driver),
        })
    }

    async fn next_event(&mut self) -> Result<Event> {
        with_timeout(self.events.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("event stream ended"))
    }

    async fn connect(&mut self) -> Result<FakeConnection> {
        let connection = self.daemon.next_connection().await?;
        match self.next_event().await? {
            Event::Connected(server_info) => assert_eq!(server_info.server_id, "srv_fake"),
            other => panic!("expected Connected, got {other:?}"),
        }
        Ok(connection)
    }
}

fn workspace_json(id: &str, status: &str) -> Value {
    json!({
        "id": id,
        "projectId": "prj_1",
        "projectDisplayName": "sandbox",
        "projectRootPath": "/tmp/sandbox",
        "projectKind": "git",
        "workspaceKind": "local_checkout",
        "name": "main",
        "status": status,
        "activityAt": null,
        "scripts": [],
    })
}

#[test]
fn hello_advertises_identity_and_capabilities() -> Result<()> {
    smol::block_on(async {
        let mut harness = Harness::start(|options| {
            options.app_version = Some("pazedo-test".into());
        })
        .await?;
        let connection = harness.connect().await?;
        assert_eq!(connection.hello["clientId"], "test-client");
        assert_eq!(connection.hello["clientType"], "mobile");
        assert_eq!(connection.hello["protocolVersion"], 1);
        assert_eq!(connection.hello["appVersion"], "pazedo-test");
        assert_eq!(
            connection.hello["capabilities"]["owned_subscriptions"],
            true
        );
        assert!(harness.client.is_connected());
        assert_eq!(
            harness.client.server_info().unwrap().version.as_deref(),
            Some("0.16.2")
        );
        Ok(())
    })
}

#[test]
fn requests_are_correlated_by_request_id() -> Result<()> {
    smol::block_on(async {
        let mut harness = Harness::start(|_| {}).await?;
        let mut connection = harness.connect().await?;

        let client = harness.client.clone();
        let workspaces = smol::spawn(async move {
            client
                .request(&FetchWorkspaces {
                    subscribe: Some(Subscribe {}),
                    ..Default::default()
                })
                .await
        });
        let client = harness.client.clone();
        let projects = smol::spawn(async move { client.request(&ListProjects {}).await });

        let mut requests = vec![
            connection.next_request().await?,
            connection.next_request().await?,
        ];
        requests.sort_by_key(|request| request["type"].as_str().map(str::to_string));
        let [fetch, list] = <[Value; 2]>::try_from(requests).unwrap();
        assert_eq!(fetch["type"], "fetch_workspaces_request");
        assert_eq!(fetch["subscribe"], json!({}));
        assert_eq!(list["type"], "project.list.request");
        assert_ne!(fetch["requestId"], list["requestId"]);

        // Answer in the opposite order to the requests.
        connection.respond(&list, "project.list.response", json!({"projects": []}))?;
        connection.respond(
            &fetch,
            "fetch_workspaces_response",
            json!({
                "subscriptionId": "sub-1",
                "entries": [workspace_json("wks_1", "running")],
                "pageInfo": {"nextCursor": null, "prevCursor": null, "hasMore": false},
            }),
        )?;

        let workspaces = with_timeout(workspaces).await??;
        assert_eq!(workspaces.subscription_id.as_deref(), Some("sub-1"));
        assert_eq!(workspaces.entries[0].status, WorkspaceStatus::Running);
        assert!(with_timeout(projects).await??.projects.is_empty());
        Ok(())
    })
}

#[test]
fn streamed_updates_arrive_with_the_response() -> Result<()> {
    smol::block_on(async {
        let mut harness = Harness::start(|_| {}).await?;
        let mut connection = harness.connect().await?;
        let client = harness.client.clone();
        let reports = smol::spawn(async move {
            client
                .request_with_updates(&ListUsageReports {
                    force_refresh: Some(true),
                })
                .await
        });

        let request = connection
            .expect_request("usage.list_reports.request")
            .await?;
        assert_eq!(request["forceRefresh"], true);
        let request_id = request["requestId"].clone();
        connection.push(
            "usage.list_reports.update",
            json!({"requestId": request_id, "report": {
                "id": "claude", "account": {"label": "me@example.com"},
                "fetchedAt": "2026-11-16T10:32:01.826Z", "sourceId": "claude",
                "sourceLabel": "Claude", "icon": "anthropic",
                "report": {"status": "available", "planLabel": "Max 5x", "windows": [
                    {"id": "five_hour", "label": "Session", "shortLabel": "5h", "summary": true,
                     "usedPct": 24, "remainingPct": 76, "resetsAt": "2026-11-16T10:50:00Z",
                     "tone": "ok"},
                    {"id": "weekly_model_fable", "label": "Weekly · Fable", "usedPct": 0,
                     "remainingPct": 100, "tone": "something new"},
                ]},
            }}),
        )?;
        // A message pushed meanwhile is still an event.
        connection.push(
            "workspace_update",
            json!({"kind": "upsert", "workspace": workspace_json("wks_1", "running")}),
        )?;
        connection.push(
            "usage.list_reports.update",
            json!({"requestId": request_id, "report": {
                "id": "codex", "account": {}, "fetchedAt": "2026-11-16T10:32:02Z",
                "sourceId": "codex", "sourceLabel": "Codex",
                "report": {"status": "unavailable", "problem": {"kind": "expired",
                    "expiresAt": "2026-11-16T09:00:00Z"}},
                "lastSuccessful": {"fetchedAt": "2026-11-16T08:00:00Z", "report": {
                    "status": "available", "windows": [], "balances": [
                        {"id": "credits", "label": "Credits", "remaining": 0,
                         "unit": "credits", "tone": "danger"}]}},
            }}),
        )?;
        connection.respond(
            &request,
            "usage.list_reports.response",
            json!({"error": null}),
        )?;

        let (response, updates) = with_timeout(reports).await??;
        assert_eq!(response.error, None);
        let [claude, codex] = <[_; 2]>::try_from(updates).unwrap();
        let claude = claude.report;
        assert_eq!(claude.icon.as_deref(), Some("anthropic"));
        let available = claude.available().expect("Claude's usage");
        assert_eq!(available.plan_label.as_deref(), Some("Max 5x"));
        assert_eq!(available.windows[0].short_label.as_deref(), Some("5h"));
        assert!(available.windows[0].summary);
        assert_eq!(available.windows[0].remaining_pct, Some(76.0));
        assert_eq!(available.windows[0].tone, UsageTone::Ok);
        assert_eq!(available.windows[1].tone, UsageTone::Default);
        let codex = codex.report;
        assert_eq!(
            codex.report,
            UsageReport::Unavailable {
                problem: UsageProblem::Expired {}
            }
        );
        assert_eq!(
            codex.available().map(|usage| usage.balances[0].tone),
            Some(UsageTone::Danger),
            "an unavailable report keeps its last data"
        );
        match harness.next_event().await? {
            Event::Message(ServerMessage::WorkspaceUpdate(_)) => {}
            other => panic!("expected the workspace update, got {other:?}"),
        }
        Ok(())
    })
}

#[test]
fn rpc_errors_fail_the_request_with_a_typed_error() -> Result<()> {
    smol::block_on(async {
        let mut harness = Harness::start(|_| {}).await?;
        let mut connection = harness.connect().await?;
        let client = harness.client.clone();
        let response = smol::spawn(async move { client.request(&ListProjects {}).await });

        let request = connection.expect_request("project.list.request").await?;
        connection.respond(
            &request,
            "rpc_error",
            json!({
                "requestType": "project.list.request",
                "error": "Unknown request, try upgrading the daemon",
                "code": "unknown_schema",
            }),
        )?;

        let error = with_timeout(response).await?.unwrap_err();
        let rpc_error = error.downcast_ref::<RpcError>().expect("an RpcError");
        assert_eq!(rpc_error.code.as_deref(), Some("unknown_schema"));
        Ok(())
    })
}

#[test]
fn pushed_messages_become_events_and_unknown_ones_are_kept_raw() -> Result<()> {
    smol::block_on(async {
        let mut harness = Harness::start(|_| {}).await?;
        let connection = harness.connect().await?;

        connection.push(
            "workspace_update",
            json!({"subscriptionId": "sub-1", "kind": "upsert", "workspace": workspace_json("wks_2", "needs_input")}),
        )?;
        connection.push(
            "workspace_update",
            json!({"subscriptionId": "sub-1", "kind": "remove", "id": "wks_2"}),
        )?;
        connection.push("brand_new_message", json!({"x": 1}))?;
        // A response to a request the client never made is delivered as an event, not dropped.
        connection.push(
            "rpc_error",
            json!({"requestId": "someone-else", "error": "nope"}),
        )?;

        match harness.next_event().await? {
            Event::Message(ServerMessage::WorkspaceUpdate(WorkspaceUpdate::Upsert {
                workspace,
                subscription_id,
            })) => {
                assert_eq!(workspace.id, "wks_2");
                assert_eq!(workspace.status, WorkspaceStatus::NeedsInput);
                assert_eq!(subscription_id.as_deref(), Some("sub-1"));
            }
            other => panic!("expected a workspace upsert, got {other:?}"),
        }
        match harness.next_event().await? {
            Event::Message(ServerMessage::WorkspaceUpdate(WorkspaceUpdate::Remove {
                id, ..
            })) => assert_eq!(id, "wks_2"),
            other => panic!("expected a workspace removal, got {other:?}"),
        }
        match harness.next_event().await? {
            Event::Message(ServerMessage::Other {
                message_type,
                payload,
            }) => {
                assert_eq!(message_type, "brand_new_message");
                assert_eq!(payload["x"], 1);
            }
            other => panic!("expected an unknown message, got {other:?}"),
        }
        assert!(matches!(
            harness.next_event().await?,
            Event::Message(ServerMessage::Other { message_type, .. }) if message_type == "rpc_error"
        ));
        Ok(())
    })
}

#[test]
fn losing_the_connection_fails_pending_requests_and_reconnects() -> Result<()> {
    smol::block_on(async {
        let mut harness = Harness::start(|_| {}).await?;
        let mut connection = harness.connect().await?;

        let client = harness.client.clone();
        let pending = smol::spawn(async move { client.request(&ListProjects {}).await });
        connection.expect_request("project.list.request").await?;
        connection.close()?;

        assert!(matches!(
            harness.next_event().await?,
            Event::Disconnected { .. }
        ));
        let error = with_timeout(pending).await?.unwrap_err();
        assert!(
            format!("{error:#}").contains("connection to the Paseo daemon lost"),
            "{error:#}"
        );

        let mut connection = harness.connect().await?;
        let client = harness.client.clone();
        let retried = smol::spawn(async move { client.request(&ListProjects {}).await });
        let request = connection.expect_request("project.list.request").await?;
        connection.respond(&request, "project.list.response", json!({"projects": []}))?;
        with_timeout(retried).await??;
        Ok(())
    })
}

#[test]
fn requests_fail_fast_while_disconnected() -> Result<()> {
    smol::block_on(async {
        let mut harness = Harness::start(|_| {}).await?;
        harness
            .daemon
            .set_hello_rejection(Some("password_required"));
        match harness.next_event().await? {
            Event::Disconnected { reason } => {
                assert!(reason.contains("password required"), "{reason}")
            }
            other => panic!("expected Disconnected, got {other:?}"),
        }
        assert!(matches!(harness.next_event().await?, Event::Rejected(_)));
        let error = harness.client.request(&ListProjects {}).await.unwrap_err();
        assert!(format!("{error:#}").contains("not connected"), "{error:#}");
        assert!(harness.client.server_info().is_none());

        harness.daemon.set_hello_rejection(None);
        harness.client.reconnect_now();
        harness.connect().await?;
        Ok(())
    })
}

#[test]
fn a_rejected_hello_waits_for_new_credentials() -> Result<()> {
    smol::block_on(async {
        let mut harness = Harness::start(|_| {}).await?;
        harness
            .daemon
            .set_hello_rejection(Some("password_required"));
        assert!(matches!(
            harness.next_event().await?,
            Event::Disconnected { .. }
        ));
        assert!(matches!(
            harness.next_event().await?,
            Event::Rejected(HelloRejectionReason::PasswordRequired)
        ));

        harness.daemon.set_hello_rejection(None);
        let retried = matches!(
            futures::future::select(
                Box::pin(harness.daemon.next_connection()),
                smol_sleep()(Duration::from_millis(300)),
            )
            .await,
            futures::future::Either::Left(_)
        );
        assert!(!retried, "no retry with the credentials that were refused");

        harness.client.set_auth(Some(HelloAuth::Password {
            password: "secret".into(),
        }));
        harness.client.reconnect_now();
        let connection = harness.connect().await?;
        assert_eq!(
            connection.hello["auth"],
            json!({"kind": "password", "password": "secret"})
        );
        assert_eq!(
            format!(
                "{:?}",
                HelloAuth::Password {
                    password: "secret".into()
                }
            ),
            "Password { .. }",
            "debug output leaves the password out"
        );
        Ok(())
    })
}

#[test]
fn hello_rejection_is_a_typed_error() -> Result<()> {
    smol::block_on(async {
        let daemon = FakeDaemon::start().await?;
        daemon.set_hello_rejection(Some("incorrect_password"));
        assert_eq!(
            HelloRejected(HelloRejectionReason::IncorrectPassword).to_string(),
            "incorrect password"
        );
        let (_client, mut events, driver) = Client::new(ConnectOptions::new(
            daemon.endpoint(),
            "test-client",
            smol_sleep(),
        ));
        let _driver = smol::spawn(driver);
        match with_timeout(events.next()).await? {
            Some(Event::Disconnected { reason }) => {
                assert!(reason.contains("incorrect password"), "{reason}")
            }
            other => panic!("expected Disconnected, got {other:?}"),
        }
        Ok(())
    })
}

#[test]
fn chunked_messages_are_reassembled() -> Result<()> {
    smol::block_on(async {
        let mut harness = Harness::start(|_| {}).await?;
        let connection = harness.connect().await?;

        let message = json!({"type": "session", "message": {"type": "brand_new_message", "payload": {"big": "x".repeat(100)}}}).to_string();
        let (first, second) = message.as_bytes().split_at(40);
        connection.send_binary([&[0x31, 1, 0, 1][..], first].concat())?;
        connection.send_binary([&[0x31, 1, 0, 2][..], second].concat())?;

        match harness.next_event().await? {
            Event::Message(ServerMessage::Other { payload, .. }) => {
                assert_eq!(payload["big"].as_str().map(str::len), Some(100))
            }
            other => panic!("expected the reassembled message, got {other:?}"),
        }
        Ok(())
    })
}

#[test]
fn terminal_frames_flow_both_ways() -> Result<()> {
    smol::block_on(async {
        let mut harness = Harness::start(|_| {}).await?;
        let mut connection = harness.connect().await?;

        connection.send_binary(vec![0x01, 3, b'h', b'i'])?;
        match harness.next_event().await? {
            Event::Terminal(frame) => assert_eq!(
                frame,
                TerminalFrame {
                    opcode: TerminalOpcode::Output,
                    slot: 3,
                    payload: b"hi".to_vec()
                }
            ),
            other => panic!("expected a terminal frame, got {other:?}"),
        }

        harness.client.send_terminal_frame(&TerminalFrame {
            opcode: TerminalOpcode::Input,
            slot: 3,
            payload: b"ls\r".to_vec(),
        })?;
        assert_eq!(
            connection.next_binary().await?,
            vec![0x02, 3, b'l', b's', b'\r']
        );

        harness.client.notify(&TerminalInput {
            terminal_id: "term-1".into(),
            message: TerminalClientMessage::Resize { rows: 24, cols: 80 },
        })?;
        let input = connection.expect_request("terminal_input").await?;
        assert_eq!(
            input,
            json!({"type": "terminal_input", "terminalId": "term-1", "message": {"type": "resize", "rows": 24, "cols": 80}})
        );
        Ok(())
    })
}

#[test]
fn subscribed_terminal_frames_go_to_their_stream_until_disconnect() -> Result<()> {
    smol::block_on(async {
        let mut harness = Harness::start(|_| {}).await?;
        let mut connection = harness.connect().await?;

        let subscribe = smol::spawn({
            let client = harness.client.clone();
            async move {
                client
                    .subscribe_terminal(&SubscribeTerminal {
                        terminal_id: "term-1".into(),
                        restore: None,
                    })
                    .await
            }
        });
        let request = connection
            .expect_request("subscribe_terminal_request")
            .await?;
        connection.respond(
            &request,
            "subscribe_terminal_response",
            json!({"terminalId": "term-1", "slot": 5, "subscriptionId": "sub-1", "error": null}),
        )?;
        // Sent right behind the response, before the subscriber has seen the slot.
        connection.send_binary(vec![0x05, 5, b'$', b' '])?;
        connection.send_binary(vec![0x01, 6, b'x'])?;

        let (response, mut frames) = with_timeout(subscribe).await??;
        assert_eq!(response.slot, Some(5));
        assert_eq!(
            with_timeout(frames.next()).await?,
            Some(TerminalFrame {
                opcode: TerminalOpcode::Restore,
                slot: 5,
                payload: b"$ ".to_vec()
            })
        );
        match harness.next_event().await? {
            Event::Terminal(frame) => assert_eq!(frame.slot, 6),
            other => panic!("expected the unrouted frame as an event, got {other:?}"),
        }

        connection.close()?;
        assert_eq!(with_timeout(frames.next()).await?, None);
        Ok(())
    })
}

#[test]
fn requests_time_out_and_late_responses_become_events() -> Result<()> {
    smol::block_on(async {
        let mut harness = Harness::start(|options| {
            options.request_timeout = Duration::from_millis(100);
        })
        .await?;
        let mut connection = harness.connect().await?;

        let error = harness.client.request(&ListProjects {}).await.unwrap_err();
        assert!(format!("{error:#}").contains("timed out"), "{error:#}");

        let request = connection.expect_request("project.list.request").await?;
        connection.respond(&request, "project.list.response", json!({"projects": []}))?;
        assert!(matches!(
            harness.next_event().await?,
            Event::Message(ServerMessage::Other { message_type, .. }) if message_type == "project.list.response"
        ));
        Ok(())
    })
}

#[test]
fn dropping_every_client_handle_stops_the_driver() -> Result<()> {
    smol::block_on(async {
        let daemon = FakeDaemon::start().await?;
        let (client, mut events, driver) = Client::new(ConnectOptions::new(
            daemon.endpoint(),
            "test-client",
            smol_sleep(),
        ));
        let driver = smol::spawn(driver);
        assert!(matches!(
            with_timeout(events.next()).await?,
            Some(Event::Connected(_))
        ));
        drop(client);
        with_timeout(driver).await?;
        Ok(())
    })
}
