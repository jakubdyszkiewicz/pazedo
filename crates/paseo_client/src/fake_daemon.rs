//! An in-process fake Paseo daemon for tests: a real WebSocket server on a loopback port that
//! answers `hello` and pings, and lets the test script every other message.

use anyhow::{Context as _, Result, anyhow, bail};
use async_tungstenite::tungstenite::Message;
use futures::{
    FutureExt as _, StreamExt as _,
    channel::mpsc,
    future::{self, Either},
    select_biased,
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use smol::{Task, net::TcpListener};
use std::{future::Future, sync::Arc, time::Duration};

use crate::{Endpoint, Sleep};

const WAIT_TIMEOUT: Duration = Duration::from_secs(10);

pub fn default_server_info() -> Value {
    json!({
        "status": "server_info",
        "protocolVersion": 1,
        "serverId": "srv_fake",
        "hostname": "fake daemon",
        "version": "0.15.4",
        "permissions": ["daemon.read", "workspace.read", "workspace.write"],
        "features": {
            "ownedSubscriptions": true,
            "binaryChunkingV1": true,
        },
    })
}

#[derive(Clone)]
enum HelloReply {
    Accept(Value),
    Reject(String),
}

pub struct FakeDaemon {
    endpoint: Endpoint,
    hello_reply: Arc<Mutex<HelloReply>>,
    connections: mpsc::UnboundedReceiver<FakeConnection>,
    _accept: Task<()>,
}

impl FakeDaemon {
    pub async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let hello_reply = Arc::new(Mutex::new(HelloReply::Accept(default_server_info())));
        let (connections_tx, connections) = mpsc::unbounded();
        let accept = smol::spawn({
            let hello_reply = hello_reply.clone();
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    match async_tungstenite::accept_async(stream).await {
                        Ok(websocket) => {
                            let reply = hello_reply.lock().clone();
                            if let Some(connection) = FakeConnection::start(websocket, reply).await
                            {
                                connections_tx.unbounded_send(connection).ok();
                            }
                        }
                        Err(error) => log::warn!("fake daemon handshake failed: {error}"),
                    }
                }
            }
        });
        Ok(Self {
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".into(),
                port,
                path: "/ws".into(),
            },
            hello_reply,
            connections,
            _accept: accept,
        })
    }

    pub fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }

    pub fn set_server_info(&self, server_info: Value) {
        *self.hello_reply.lock() = HelloReply::Accept(server_info);
    }

    /// Rejects later `hello`s with this `hello.rejected` reason, or accepts them again on `None`.
    pub fn set_hello_rejection(&self, reason: Option<&str>) {
        *self.hello_reply.lock() = match reason {
            Some(reason) => HelloReply::Reject(reason.to_string()),
            None => HelloReply::Accept(default_server_info()),
        };
    }

    /// The next connection whose `hello` was accepted.
    pub async fn next_connection(&mut self) -> Result<FakeConnection> {
        with_timeout(self.connections.next())
            .await?
            .context("fake daemon stopped accepting connections")
    }
}

pub enum Inbound {
    /// The inner message of a `session` envelope, including `type` and `requestId`.
    Session(Value),
    Binary(Vec<u8>),
}

pub struct FakeConnection {
    pub hello: Value,
    inbound: mpsc::UnboundedReceiver<Inbound>,
    outgoing: mpsc::UnboundedSender<Message>,
    _task: Task<()>,
}

impl FakeConnection {
    /// Reads `hello` and replies. Returns `None` if the hello was rejected or never arrived.
    async fn start<S>(
        websocket: async_tungstenite::WebSocketStream<S>,
        reply: HelloReply,
    ) -> Option<Self>
    where
        S: futures::AsyncRead + futures::AsyncWrite + Send + Unpin + 'static,
    {
        let (mut sink, mut stream) = websocket.split();
        let hello = loop {
            match stream.next().await? {
                Ok(Message::Text(text)) => {
                    let message: Value = serde_json::from_str(&text).ok()?;
                    if message["type"] == "hello" {
                        break message;
                    }
                }
                Ok(_) => continue,
                Err(_) => return None,
            }
        };
        match reply {
            HelloReply::Reject(reason) => {
                let rejection =
                    json!({"type": "hello.rejected", "reason": reason, "accepts": ["password"]});
                sink.send(Message::text(rejection.to_string())).await.ok();
                sink.close(None).await.ok();
                return None;
            }
            HelloReply::Accept(server_info) => {
                let message = json!({"type": "session", "message": {"type": "status", "payload": server_info}});
                sink.send(Message::text(message.to_string())).await.ok()?;
            }
        }

        let (inbound_tx, inbound) = mpsc::unbounded();
        let (outgoing, mut outgoing_rx) = mpsc::unbounded::<Message>();
        let task = smol::spawn(async move {
            loop {
                select_biased! {
                    message = outgoing_rx.next() => {
                        let Some(message) = message else { break };
                        let is_close = matches!(message, Message::Close(_));
                        if sink.send(message).await.is_err() || is_close {
                            break;
                        }
                    }
                    message = stream.next().fuse() => {
                        let message = match message {
                            Some(Ok(message)) => message,
                            _ => break,
                        };
                        match message {
                            Message::Text(text) => {
                                let Ok(message) = serde_json::from_str::<Value>(&text) else {
                                    continue;
                                };
                                if message["type"] == "ping" {
                                    if sink.send(Message::text(r#"{"type":"pong"}"#)).await.is_err() {
                                        break;
                                    }
                                } else if message["type"] == "session" {
                                    inbound_tx
                                        .unbounded_send(Inbound::Session(message["message"].clone()))
                                        .ok();
                                }
                            }
                            Message::Binary(bytes) => {
                                inbound_tx.unbounded_send(Inbound::Binary(bytes.to_vec())).ok();
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                }
            }
        });
        Some(Self {
            hello,
            inbound,
            outgoing,
            _task: task,
        })
    }

    pub async fn next_inbound(&mut self) -> Result<Inbound> {
        with_timeout(self.inbound.next())
            .await?
            .context("client disconnected")
    }

    /// The next session message from the client, which must not be binary.
    pub async fn next_request(&mut self) -> Result<Value> {
        match self.next_inbound().await? {
            Inbound::Session(message) => Ok(message),
            Inbound::Binary(_) => bail!("expected a session message, got a binary frame"),
        }
    }

    pub async fn expect_request(&mut self, message_type: &str) -> Result<Value> {
        let request = self.next_request().await?;
        if request["type"] != message_type {
            bail!("expected {message_type}, got {request}");
        }
        Ok(request)
    }

    pub async fn next_binary(&mut self) -> Result<Vec<u8>> {
        match self.next_inbound().await? {
            Inbound::Binary(bytes) => Ok(bytes),
            Inbound::Session(message) => bail!("expected a binary frame, got {message}"),
        }
    }

    /// Answers `request` with `response_type`, copying its `requestId` into the payload.
    pub fn respond(&self, request: &Value, response_type: &str, mut payload: Value) -> Result<()> {
        payload["requestId"] = request["requestId"].clone();
        self.push(response_type, payload)
    }

    pub fn push(&self, message_type: &str, payload: Value) -> Result<()> {
        let message =
            json!({"type": "session", "message": {"type": message_type, "payload": payload}});
        self.send_text(message.to_string())
    }

    pub fn send_text(&self, text: String) -> Result<()> {
        self.send(Message::text(text))
    }

    pub fn send_binary(&self, bytes: Vec<u8>) -> Result<()> {
        self.send(Message::binary(bytes))
    }

    pub fn close(&self) -> Result<()> {
        self.send(Message::Close(None))
    }

    fn send(&self, message: Message) -> Result<()> {
        self.outgoing
            .unbounded_send(message)
            .map_err(|_| anyhow!("client disconnected"))
    }
}

/// Real-time sleep for code that does not run on a gpui executor: this crate's tests and the
/// debug example. gpui code must pass `BackgroundExecutor::timer` instead.
#[allow(clippy::disallowed_methods)]
pub fn smol_sleep() -> Sleep {
    Arc::new(|duration| {
        async move {
            smol::Timer::after(duration).await;
        }
        .boxed()
    })
}

pub async fn with_timeout<T>(future: impl Future<Output = T>) -> Result<T> {
    let timer = smol_sleep()(WAIT_TIMEOUT);
    futures::pin_mut!(future);
    match future::select(future, timer).await {
        Either::Left((value, _)) => Ok(value),
        Either::Right(_) => Err(anyhow!("timed out after {WAIT_TIMEOUT:?}")),
    }
}
