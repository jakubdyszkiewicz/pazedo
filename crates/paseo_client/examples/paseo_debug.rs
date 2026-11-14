//! Manual checks against a running daemon.
//!
//!   cargo run -p paseo_client --example paseo_debug -- [--endpoint 127.0.0.1:6868] <command>
//!
//! Commands: `info`, `workspaces`, `agents`, `watch [seconds]`, `timeline <agent-id> [seconds]`,
//! `terminals <cwd> [workspace-id]`, `terminal-stream <cwd> [terminal-id] [restore-mode]`. The endpoint defaults to the isolated test daemon
//! (docs/pazedo/development.md), never to the user's real daemon.

use anyhow::{Context as _, Result, bail};
use futures::{StreamExt as _, channel::mpsc};
use paseo_client::{
    Client, ConnectOptions, Endpoint, Event,
    fake_daemon::smol_sleep,
    frames::{TerminalFrame, TerminalOpcode},
    protocol::{
        AgentFilter, AgentSessionConfig, AgentStreamEvent, AgentUpdate, CreateAgent,
        CreateTerminal, FetchAgentTimeline, FetchAgents, FetchWorkspaces, ListProjects,
        PermissionResponse, ProjectUpdate, RespondToPermission, SendAgentMessage, ServerMessage,
        SetAgentTimelineSubscription, SetSessionEventSubscription, Subscribe, SubscribeTerminal,
        SubscribeTerminals, TerminalRestore, TerminalRestoreMode, TerminalSize, TimelineDirection,
        TimelineProjection, WorkspaceUpdate,
    },
};
use std::time::{Duration, Instant};

const DEFAULT_ENDPOINT: &str = "127.0.0.1:6868";

fn main() -> Result<()> {
    env_logger::init();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let endpoint = match args.iter().position(|arg| arg == "--endpoint") {
        Some(index) => {
            args.remove(index);
            if index >= args.len() {
                bail!("--endpoint needs a value");
            }
            args.remove(index)
        }
        None => DEFAULT_ENDPOINT.to_string(),
    };
    let endpoint = Endpoint::parse(&endpoint)?;
    let command = args.first().cloned().unwrap_or_else(|| "info".into());

    smol::block_on(async move {
        let mut options = ConnectOptions::new(endpoint.clone(), "pazedo-debug", smol_sleep());
        options.app_version = Some(env!("CARGO_PKG_VERSION").into());
        let (client, mut events, driver) = Client::new(options);
        let _driver = smol::spawn(driver);
        match events.next().await {
            Some(Event::Connected(info)) => println!(
                "connected to {endpoint}: server {} ({}), Paseo {}",
                info.server_id,
                info.hostname.as_deref().unwrap_or("?"),
                info.version.as_deref().unwrap_or("?")
            ),
            Some(Event::Disconnected { reason }) => bail!("could not connect: {reason}"),
            other => bail!("unexpected first event: {other:?}"),
        }

        match command.as_str() {
            "info" => Ok(()),
            "workspaces" => list_workspaces(&client).await,
            "agents" => list_agents(&client).await,
            "watch" => watch(&client, &mut events, seconds_arg(&args, 1)?).await,
            "timeline" => {
                let agent_id = args.get(1).context("timeline needs an agent id")?.clone();
                timeline(&client, &mut events, agent_id, seconds_arg(&args, 2)?).await
            }
            "create" => {
                let cwd = args.get(1).context("create needs a cwd")?.clone();
                let prompt = args.get(2).context("create needs a prompt")?.clone();
                let response = client
                    .request(&CreateAgent {
                        config: AgentSessionConfig {
                            provider: "mock".into(),
                            cwd,
                            model: Some("ten-second-stream".into()),
                            title: Some("paseo_debug".into()),
                            ..Default::default()
                        },
                        initial_prompt: Some(prompt),
                        ..Default::default()
                    })
                    .await?;
                match (response.agent, response.error) {
                    (Some(agent), _) => println!("created agent {} ({:?})", agent.id, agent.status),
                    (None, error) => bail!("create failed: {error:?}"),
                }
                Ok(())
            }
            "send" => {
                let agent_id = args.get(1).context("send needs an agent id")?.clone();
                let text = args.get(2).context("send needs a text")?.clone();
                let response = client
                    .request(&SendAgentMessage {
                        agent_id,
                        text,
                        message_id: args.get(3).cloned(),
                        active_turn_behavior: None,
                    })
                    .await?;
                println!("{response:?}");
                Ok(())
            }
            "respond" => {
                let agent_id = args.get(1).context("respond needs an agent id")?.clone();
                let request_id = args
                    .get(2)
                    .context("respond needs a permission id")?
                    .clone();
                let action_id = args.get(3).cloned();
                client.notify(&RespondToPermission {
                    agent_id,
                    request_id,
                    response: PermissionResponse::Allow {
                        selected_action_id: action_id,
                    },
                })?;
                // Give the notification time to be written before the process exits.
                smol_sleep()(Duration::from_millis(500)).await;
                println!("sent");
                Ok(())
            }
            "terminals" => {
                let cwd = args.get(1).context("terminals needs a cwd")?.clone();
                let terminals = client
                    .request(&SubscribeTerminals {
                        cwd,
                        workspace_id: args.get(2).cloned(),
                    })
                    .await?;
                println!(
                    "workspace {:?}, subscription {:?}",
                    terminals.workspace_id, terminals.subscription_id
                );
                for terminal in terminals.terminals {
                    println!(
                        "{}  {}  {:?}  workspace={:?}",
                        terminal.id, terminal.name, terminal.title, terminal.workspace_id
                    );
                }
                Ok(())
            }
            "terminal-stream" => {
                let cwd = args.get(1).context("terminal-stream needs a cwd")?.clone();
                terminal_stream(&client, &mut events, cwd, args.get(2).cloned(), args.get(3)).await
            }
            other => bail!("unknown command {other}"),
        }
    })
}

/// Attaches to a terminal (creating one when no id is given), types a command into it and
/// prints every frame of the stream.
async fn terminal_stream(
    client: &Client,
    events: &mut mpsc::UnboundedReceiver<Event>,
    cwd: String,
    terminal_id: Option<String>,
    restore_mode: Option<&String>,
) -> Result<()> {
    let size = TerminalSize { rows: 24, cols: 80 };
    let terminal_id = match terminal_id {
        Some(terminal_id) => terminal_id,
        None => {
            let response = client
                .request(&CreateTerminal {
                    cwd,
                    name: Some("paseo_debug".into()),
                    size: Some(size),
                    ..Default::default()
                })
                .await?;
            let terminal = response
                .terminal
                .with_context(|| format!("create failed: {:?}", response.error))?;
            println!("created terminal {}", terminal.id);
            terminal.id
        }
    };
    let mode = match restore_mode.map(String::as_str) {
        None | Some("visible") => TerminalRestoreMode::VisibleSnapshot,
        Some("full") => TerminalRestoreMode::FullSnapshot,
        Some("live") => TerminalRestoreMode::Live,
        Some(other) => bail!("unknown restore mode {other}"),
    };
    let response = client
        .request(&SubscribeTerminal {
            terminal_id: terminal_id.clone(),
            restore: Some(TerminalRestore {
                mode,
                scrollback_lines: None,
                size: Some(size),
            }),
        })
        .await?;
    println!("subscribed: {response:?}");
    let slot = response.slot.context("no slot")?;
    print_frames(events, Instant::now() + Duration::from_secs(3)).await;
    client.send_terminal_frame(&TerminalFrame {
        opcode: TerminalOpcode::Input,
        slot,
        payload: b"echo pazedo-$((6*7))\r".to_vec(),
    })?;
    println!("sent input");
    print_frames(events, Instant::now() + Duration::from_secs(3)).await;
    Ok(())
}

async fn print_frames(events: &mut mpsc::UnboundedReceiver<Event>, deadline: Instant) {
    while let Some(event) = next_event_before(events, deadline).await {
        print_event(event);
    }
}

fn print_event(event: Event) {
    match event {
        Event::Terminal(frame) => println!(
            "frame {:?} slot {} {} bytes: {:?}",
            frame.opcode,
            frame.slot,
            frame.payload.len(),
            String::from_utf8_lossy(&frame.payload[..frame.payload.len().min(300)])
        ),
        Event::Message(message) => println!("message {message:?}"),
        other => println!("{other:?}"),
    }
}

fn seconds_arg(args: &[String], index: usize) -> Result<Duration> {
    Ok(Duration::from_secs(match args.get(index) {
        Some(seconds) => seconds.parse().context("invalid number of seconds")?,
        None => 30,
    }))
}

async fn list_workspaces(client: &Client) -> Result<()> {
    let projects = client.request(&ListProjects {}).await?;
    println!("{} projects", projects.projects.len());
    for project in &projects.projects {
        println!(
            "  {}  {}  {}",
            project.project_id, project.project_display_name, project.project_root_path
        );
    }
    let workspaces = client
        .request(&FetchWorkspaces {
            include_background: Some(true),
            ..Default::default()
        })
        .await?;
    println!("{} workspaces", workspaces.entries.len());
    for workspace in &workspaces.entries {
        println!(
            "  {}  {:?}  {} / {}  {}",
            workspace.id,
            workspace.status,
            workspace.project_display_name,
            workspace.name,
            workspace.directory()
        );
    }
    Ok(())
}

async fn list_agents(client: &Client) -> Result<()> {
    for (label, request) in [
        ("default", FetchAgents::default()),
        (
            "including background and archived",
            FetchAgents {
                include_background: Some(true),
                filter: Some(AgentFilter {
                    include_archived: Some(true),
                    include_background: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ),
    ] {
        let agents = client.request(&request).await?;
        println!("{} agents ({label})", agents.entries.len());
        for entry in &agents.entries {
            let agent = &entry.agent;
            println!(
                "  {}  {:?}  {}  attention={} {:?}  workspace={:?}  {}",
                agent.id,
                agent.status,
                agent.provider,
                agent.requires_attention,
                agent.attention_reason,
                agent.workspace_id,
                agent.title.as_deref().unwrap_or("")
            );
            for permission in &agent.pending_permissions {
                println!(
                    "    pending permission id={} kind={:?} title={:?} actions={:?}",
                    permission.id,
                    permission.kind,
                    permission.title,
                    permission
                        .actions
                        .iter()
                        .map(|action| (&action.id, action.behavior))
                        .collect::<Vec<_>>()
                );
            }
        }
    }
    Ok(())
}

async fn watch(
    client: &Client,
    events: &mut mpsc::UnboundedReceiver<Event>,
    duration: Duration,
) -> Result<()> {
    subscribe_directory(client).await?;
    let deadline = Instant::now() + duration;
    while let Some(event) = next_event_before(events, deadline).await {
        match event {
            Event::Connected(_) => {
                println!("reconnected; subscribing again");
                subscribe_directory(client).await?;
            }
            Event::Disconnected { reason } => println!("disconnected: {reason}"),
            Event::Message(message) => print_message(&message),
            Event::Terminal(frame) => {
                println!("terminal frame {:?} slot {}", frame.opcode, frame.slot)
            }
        }
    }
    Ok(())
}

async fn subscribe_directory(client: &Client) -> Result<()> {
    let workspaces = client
        .request(&FetchWorkspaces {
            include_background: Some(true),
            subscribe: Some(Subscribe {}),
            ..Default::default()
        })
        .await?;
    let agents = client
        .request(&FetchAgents {
            include_background: Some(true),
            subscribe: Some(Subscribe {}),
            ..Default::default()
        })
        .await?;
    let session_events = client
        .request(&SetSessionEventSubscription {
            events: [
                "project.update",
                "agent_attention_required",
                "agent_permission_request",
                "agent_permission_resolved",
            ]
            .map(String::from)
            .to_vec(),
            notifications: None,
        })
        .await?;
    println!(
        "watching {} workspaces (subscription {:?}), {} agents ({:?}), events ({:?})",
        workspaces.entries.len(),
        workspaces.subscription_id,
        agents.entries.len(),
        agents.subscription_id,
        session_events.subscription_id,
    );
    Ok(())
}

fn print_message(message: &ServerMessage) {
    match message {
        ServerMessage::WorkspaceUpdate(WorkspaceUpdate::Upsert { workspace, .. }) => println!(
            "workspace upsert {} {:?} {}",
            workspace.id, workspace.status, workspace.name
        ),
        ServerMessage::WorkspaceUpdate(WorkspaceUpdate::Remove { id, .. }) => {
            println!("workspace remove {id}")
        }
        ServerMessage::AgentUpdate(AgentUpdate::Upsert { agent, .. }) => println!(
            "agent upsert {} {:?} attention={} {:?} {}",
            agent.id,
            agent.status,
            agent.requires_attention,
            agent.attention_reason,
            agent.title.as_deref().unwrap_or("")
        ),
        ServerMessage::AgentUpdate(AgentUpdate::Remove { agent_id, .. }) => {
            println!("agent remove {agent_id}")
        }
        ServerMessage::ProjectUpdate(ProjectUpdate::Upsert { project, .. }) => {
            println!(
                "project upsert {} {}",
                project.project_id, project.project_display_name
            )
        }
        ServerMessage::ProjectUpdate(ProjectUpdate::Remove { project_id, .. }) => {
            println!("project remove {project_id}")
        }
        ServerMessage::AgentStream(stream) => println!(
            "agent_stream {} {} seq={:?}",
            stream.agent_id,
            stream.event_type().unwrap_or("?"),
            stream.seq
        ),
        ServerMessage::Other { message_type, .. } => println!("{message_type} (not modelled)"),
        other => println!("{other:?}"),
    }
}

async fn timeline(
    client: &Client,
    events: &mut mpsc::UnboundedReceiver<Event>,
    agent_id: String,
    duration: Duration,
) -> Result<()> {
    let subscription = client
        .request(&SetAgentTimelineSubscription {
            agent_ids: vec![agent_id.clone()],
        })
        .await?;
    let history = client
        .request(&FetchAgentTimeline {
            agent_id: agent_id.clone(),
            direction: TimelineDirection::Tail,
            cursor: None,
            limit: Some(20),
            projection: TimelineProjection::Projected,
            whole_turns: Some(true),
        })
        .await?;
    println!(
        "timeline epoch {} ({} entries, has_older={}), live subscription {:?}",
        history.epoch,
        history.entries.len(),
        history.has_older,
        subscription.subscription_id
    );
    for entry in &history.entries {
        println!(
            "  [{}..{}] {}: {} {}",
            entry.seq_start,
            entry.seq_end,
            entry.item_type().unwrap_or("?"),
            preview(&entry.item),
            ["messageId", "clientMessageId"]
                .iter()
                .filter_map(|key| entry.item.get(*key).map(|value| format!("{key}={value}")))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    let deadline = Instant::now() + duration;
    while let Some(event) = next_event_before(events, deadline).await {
        if let Event::Message(ServerMessage::AgentStream(stream)) = event {
            match stream.event() {
                AgentStreamEvent::PermissionRequested(request) => println!(
                    "  live permission request id={} kind={:?} title={:?} actions={:?}",
                    request.id,
                    request.kind,
                    request.title,
                    request
                        .actions
                        .iter()
                        .map(|action| (&action.id, &action.label, action.behavior))
                        .collect::<Vec<_>>()
                ),
                AgentStreamEvent::PermissionResolved {
                    request_id,
                    resolution,
                } => {
                    println!("  live permission resolved id={request_id} {resolution}")
                }
                _ => println!(
                    "  live seq={:?} {}: {}",
                    stream.seq,
                    stream.event_type().unwrap_or("?"),
                    preview(&stream.event)
                ),
            }
        }
    }
    Ok(())
}

fn preview(value: &serde_json::Value) -> String {
    let text = value
        .get("text")
        .or_else(|| value.get("item").and_then(|item| item.get("text")))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string());
    text.chars()
        .take(100)
        .collect::<String>()
        .replace('\n', " ")
}

async fn next_event_before(
    events: &mut mpsc::UnboundedReceiver<Event>,
    deadline: Instant,
) -> Option<Event> {
    let remaining = deadline.checked_duration_since(Instant::now())?;
    let timer = smol_sleep()(remaining);
    futures::pin_mut!(timer);
    match futures::future::select(events.next(), timer).await {
        futures::future::Either::Left((event, _)) => event,
        futures::future::Either::Right(_) => None,
    }
}
