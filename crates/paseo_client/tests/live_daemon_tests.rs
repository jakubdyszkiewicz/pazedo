//! Checks against a real daemon. Ignored by default; run against the isolated test daemon
//! (docs/pazedo/development.md) with:
//!
//!   cargo test -p paseo_client --test live_daemon_tests -- --ignored
//!
//! `PASEO_TEST_ENDPOINT` overrides the endpoint (default `127.0.0.1:6868`).

use anyhow::Result;
use futures::StreamExt as _;
use paseo_client::{
    Client, ConnectOptions, Endpoint, Event, RpcError,
    fake_daemon::{smol_sleep, with_timeout},
    protocol::{
        FetchAgents, FetchWorkspaces, ListProjects, PASEO_PROTOCOL_PACKAGE_VERSION,
        SetSessionEventSubscription, Subscribe,
    },
};
use serde_json::json;

fn connect() -> Result<(
    Client,
    futures::channel::mpsc::UnboundedReceiver<Event>,
    smol::Task<()>,
)> {
    let endpoint = std::env::var("PASEO_TEST_ENDPOINT").unwrap_or_else(|_| "127.0.0.1:6868".into());
    let (client, events, driver) = Client::new(ConnectOptions::new(
        Endpoint::parse(&endpoint)?,
        "pazedo-live-test",
        smol_sleep(),
    ));
    Ok((client, events, smol::spawn(driver)))
}

#[test]
#[ignore = "needs a running Paseo test daemon"]
fn decodes_the_live_directory() -> Result<()> {
    smol::block_on(async {
        let (client, mut events, _driver) = connect()?;
        let server_info = match with_timeout(events.next()).await? {
            Some(Event::Connected(server_info)) => server_info,
            other => panic!("expected Connected, got {other:?}"),
        };
        assert_eq!(
            server_info.version.as_deref(),
            Some(PASEO_PROTOCOL_PACKAGE_VERSION),
            "the protocol types were written against this Paseo version"
        );
        assert!(server_info.has_feature("ownedSubscriptions"));

        // Typed decoding fails the request if any entry does not match the hand-written types.
        let projects = client.request(&ListProjects {}).await?;
        let workspaces = client
            .request(&FetchWorkspaces {
                include_background: Some(true),
                subscribe: Some(Subscribe {}),
                ..Default::default()
            })
            .await?;
        assert!(workspaces.subscription_id.is_some());
        let agents = client
            .request(&FetchAgents {
                include_background: Some(true),
                subscribe: Some(Subscribe {}),
                ..Default::default()
            })
            .await?;
        assert!(agents.subscription_id.is_some());
        let session_events = client
            .request(&SetSessionEventSubscription {
                events: vec!["project.update".into(), "agent_attention_required".into()],
                notifications: None,
            })
            .await?;
        assert!(session_events.subscription_id.is_some());
        println!(
            "{} projects, {} workspaces, {} agents",
            projects.projects.len(),
            workspaces.entries.len(),
            agents.entries.len()
        );
        Ok(())
    })
}

#[test]
#[ignore = "needs a running Paseo test daemon"]
fn unknown_requests_fail_without_dropping_the_connection() -> Result<()> {
    smol::block_on(async {
        let (client, mut events, _driver) = connect()?;
        assert!(matches!(
            with_timeout(events.next()).await?,
            Some(Event::Connected(_))
        ));

        let error = client
            .request_raw("pazedo.no_such.request", json!({}))
            .await
            .unwrap_err();
        let rpc_error = error.downcast_ref::<RpcError>().expect("an RpcError");
        assert_eq!(rpc_error.code.as_deref(), Some("unknown_schema"));

        client.request(&ListProjects {}).await?;
        assert!(client.is_connected());
        Ok(())
    })
}
