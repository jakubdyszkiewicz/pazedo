//! A client for the Paseo daemon's WebSocket protocol.
//!
//! [`Client::new`] returns a cloneable handle, a stream of [`Event`]s, and a driver future that
//! the caller spawns on any executor. The driver owns the connection: it sends `hello`, waits for
//! `server_info`, routes responses to pending requests by `requestId`, keeps the connection alive
//! with pings, and reconnects with backoff. It stops when every [`Client`] handle is dropped.
//!
//! Subscriptions belong to one connection. After every [`Event::Connected`] the consumer must
//! request them again; the daemon assigns new subscription ids. Terminal streams attached with
//! [`Client::subscribe_terminal`] end (their receiver closes) when the connection is lost.

pub mod frames;
pub mod protocol;

#[cfg(any(test, feature = "test-support"))]
pub mod fake_daemon;

use anyhow::{Context as _, Result, anyhow, bail};
use async_tungstenite::tungstenite::{Message, client::IntoClientRequest as _};
use collections::{BTreeMap, HashMap};
use futures::{
    AsyncRead, AsyncWrite, FutureExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
    future::BoxFuture,
    select_biased,
};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;
use std::{
    fmt,
    future::Future,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    frames::{ChunkAssembler, Frame, TerminalFrame},
    protocol::{
        ClientType, Hello, HelloAuth, HelloRejectionReason, Notification, PROTOCOL_VERSION,
        Request, RpcErrorPayload, ServerInfo, ServerMessage, StreamingRequest, SubscribeTerminal,
        SubscribeTerminalResponse, capabilities,
    },
};

pub const DEFAULT_PORT: u16 = 6767;
const DEFAULT_PATH: &str = "/ws";
/// The daemon closes sockets that have not sent `hello` within 15 seconds.
const HELLO_TIMEOUT: Duration = Duration::from_secs(15);

/// Where a daemon listens. Mirrors the daemon's `daemon.listen` formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Tcp {
        host: String,
        port: u16,
        path: String,
    },
    Unix(PathBuf),
}

impl Endpoint {
    /// Accepts `host:port`, `host`, `[::1]:port`, `ws://host:port/path`, `/path/to/socket` and
    /// `unix:///path/to/socket`.
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if let Some(path) = input.strip_prefix("unix://") {
            return Ok(Self::Unix(PathBuf::from(path)));
        }
        if input.starts_with('/') {
            return Ok(Self::Unix(PathBuf::from(input)));
        }
        if input.starts_with("wss://") {
            bail!("TLS (wss://) endpoints are not supported yet: {input}");
        }
        let rest = input.strip_prefix("ws://").unwrap_or(input);
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, DEFAULT_PATH),
        };
        let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
            let (host, after) = bracketed
                .split_once(']')
                .with_context(|| format!("unterminated IPv6 address in {input}"))?;
            (host, after.strip_prefix(':'))
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            }
        };
        if host.is_empty() {
            bail!("missing host in Paseo endpoint {input}");
        }
        let port = match port {
            Some(port) => port
                .parse()
                .with_context(|| format!("invalid port in Paseo endpoint {input}"))?,
            None => DEFAULT_PORT,
        };
        Ok(Self::Tcp {
            host: host.to_string(),
            port,
            path: path.to_string(),
        })
    }

    fn websocket_url(&self) -> String {
        match self {
            Self::Tcp { host, port, path } if host.contains(':') => {
                format!("ws://[{host}]:{port}{path}")
            }
            Self::Tcp { host, port, path } => format!("ws://{host}:{port}{path}"),
            // The URL only shapes the HTTP upgrade request; the socket is already connected.
            Self::Unix(_) => format!("ws://localhost{DEFAULT_PATH}"),
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp { .. } => formatter.write_str(&self.websocket_url()),
            Self::Unix(path) => write!(formatter, "unix://{}", path.display()),
        }
    }
}

/// Creates a future that completes after the duration. Inside Zed this must come from gpui's
/// `BackgroundExecutor::timer` so tests stay deterministic; outside gpui (the debug example and
/// this crate's tests) use [`fake_daemon::smol_sleep`].
pub type Sleep = Arc<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Clone)]
pub struct ConnectOptions {
    pub endpoint: Endpoint,
    /// Stable per installation; the daemon uses it to recognise returning clients.
    pub client_id: String,
    pub client_type: ClientType,
    pub auth: Option<HelloAuth>,
    pub app_version: Option<String>,
    pub capabilities: BTreeMap<String, bool>,
    pub request_timeout: Duration,
    pub ping_interval: Duration,
    pub reconnect_base_delay: Duration,
    pub reconnect_max_delay: Duration,
    pub sleep: Sleep,
}

impl fmt::Debug for ConnectOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectOptions")
            .field("endpoint", &self.endpoint)
            .field("client_id", &self.client_id)
            .field("client_type", &self.client_type)
            .field("app_version", &self.app_version)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl ConnectOptions {
    /// Defaults match Paseo's own client: ping every 10 s, reconnect after
    /// `min(1.5 s * 2^attempt, 30 s)`.
    pub fn new(endpoint: Endpoint, client_id: impl Into<String>, sleep: Sleep) -> Self {
        Self {
            endpoint,
            client_id: client_id.into(),
            client_type: ClientType::Mobile,
            auth: None,
            app_version: None,
            capabilities: default_capabilities(),
            request_timeout: Duration::from_secs(30),
            ping_interval: Duration::from_secs(10),
            reconnect_base_delay: Duration::from_millis(1500),
            reconnect_max_delay: Duration::from_secs(30),
            sleep,
        }
    }

    fn reconnect_delay(&self, attempt: u32) -> Duration {
        self.reconnect_base_delay
            .saturating_mul(2u32.saturating_pow(attempt))
            .min(self.reconnect_max_delay)
    }
}

/// Only capabilities whose wire changes this crate handles. Advertising one the client does not
/// handle changes what the daemon sends, so add them deliberately.
pub fn default_capabilities() -> BTreeMap<String, bool> {
    [
        capabilities::HELLO_REJECTION,
        capabilities::OWNED_SUBSCRIPTIONS,
        capabilities::EXPLICIT_EVENT_SUBSCRIPTIONS,
        capabilities::PROJECT_UPDATES,
        capabilities::ALL_PROVIDERS,
        capabilities::BINARY_CHUNKING_V1,
        capabilities::SELECTIVE_AGENT_TIMELINE,
    ]
    .into_iter()
    .map(|name| (name.to_string(), true))
    .collect()
}

#[derive(Debug, Clone)]
pub enum Event {
    /// The daemon accepted `hello`. Sent again after every reconnect; subscriptions from the
    /// previous connection are gone and must be requested again.
    Connected(Arc<ServerInfo>),
    /// The connection attempt failed or an established connection was lost. Pending requests
    /// have failed. The driver retries after a backoff.
    Disconnected {
        reason: String,
    },
    Message(ServerMessage),
    Terminal(TerminalFrame),
    /// The daemon refused the credentials (follows `Disconnected`). The driver does not retry
    /// with the same credentials; it waits for [`Client::reconnect_now`], typically after
    /// [`Client::set_auth`].
    Rejected(HelloRejectionReason),
}

/// A correlated response before typed decoding.
#[derive(Debug, Clone, Deserialize)]
pub struct RawMessage {
    #[serde(rename = "type")]
    pub message_type: String,
    #[serde(default)]
    pub payload: Value,
}

/// The daemon answered a request with `rpc_error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    pub request_type: Option<String>,
    pub code: Option<String>,
    pub message: String,
}

impl fmt::Display for RpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let request_type = self.request_type.as_deref().unwrap_or("request");
        match &self.code {
            Some(code) => write!(
                formatter,
                "{request_type} failed ({code}): {}",
                self.message
            ),
            None => write!(formatter, "{request_type} failed: {}", self.message),
        }
    }
}

impl std::error::Error for RpcError {}

/// The daemon refused `hello`, for example because a password is required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloRejected(pub HelloRejectionReason);

impl fmt::Display for HelloRejected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            HelloRejectionReason::PasswordRequired => formatter.write_str("password required"),
            HelloRejectionReason::IncorrectPassword => formatter.write_str("incorrect password"),
            HelloRejectionReason::IncompatibleProtocol => {
                formatter.write_str("incompatible protocol version")
            }
            HelloRejectionReason::Unknown => formatter.write_str("rejected by the daemon"),
        }
    }
}

impl std::error::Error for HelloRejected {}

struct PendingResponse {
    response: oneshot::Sender<Result<RawMessage>>,
    /// For `subscribe_terminal_request`: where the frames of the slot in the response go.
    terminal_route: Option<mpsc::UnboundedSender<TerminalFrame>>,
    /// For a [`StreamingRequest`]: where its updates go until the response arrives.
    update_route: Option<UpdateRoute>,
}

struct UpdateRoute {
    message_type: &'static str,
    payloads: mpsc::UnboundedSender<Value>,
}

struct Connection {
    outgoing: mpsc::UnboundedSender<Message>,
    server_info: Arc<ServerInfo>,
}

struct Shared {
    /// Request ids must be unique beyond this process: the daemon deduplicates agent creation
    /// by client id and request id, so ids that restart at zero after a restart look like
    /// replays of earlier requests (`agent_request_key_conflict`).
    request_id_prefix: String,
    /// `pending` entries are only added while holding this lock and seeing `Some`, and the
    /// driver clears it before failing all pending requests, so no request is left waiting for
    /// a connection that is already gone.
    connection: Mutex<Option<Connection>>,
    pending: Mutex<HashMap<String, PendingResponse>>,
    /// Terminal stream frames by slot. Routes are installed by the driver while it handles the
    /// subscribe response, so the restore frame that follows it cannot be missed.
    terminal_routes: Mutex<HashMap<u8, mpsc::UnboundedSender<TerminalFrame>>>,
    next_request_id: AtomicU64,
    /// Sent with every `hello`; starts as [`ConnectOptions::auth`].
    auth: Mutex<Option<HelloAuth>>,
    /// Wakes the driver from its wait between attempts.
    reconnect: mpsc::UnboundedSender<()>,
}

impl Shared {
    fn send(&self, message: Message) -> Result<()> {
        let connection = self.connection.lock();
        let connection = connection
            .as_ref()
            .context("not connected to the Paseo daemon")?;
        connection
            .outgoing
            .unbounded_send(message)
            .map_err(|_| anyhow!("connection to the Paseo daemon closed"))
    }

    fn disconnected(&self, reason: &str) {
        self.connection.lock().take();
        self.terminal_routes.lock().clear();
        for (_, pending) in self.pending.lock().drain() {
            pending
                .response
                .send(Err(anyhow!(
                    "connection to the Paseo daemon lost: {reason}"
                )))
                .ok();
        }
    }
}

#[derive(Clone)]
pub struct Client {
    shared: Arc<Shared>,
    request_timeout: Duration,
    sleep: Sleep,
    /// Dropped together with the last handle, which stops the driver.
    _shutdown: Arc<oneshot::Sender<()>>,
}

impl Client {
    pub fn new(
        options: ConnectOptions,
    ) -> (
        Self,
        mpsc::UnboundedReceiver<Event>,
        impl Future<Output = ()> + Send + 'static,
    ) {
        let (reconnect_tx, reconnect_rx) = mpsc::unbounded();
        let shared = Arc::new(Shared {
            request_id_prefix: format!("pazedo-{}", uuid::Uuid::new_v4().simple()),
            connection: Mutex::default(),
            pending: Mutex::default(),
            terminal_routes: Mutex::default(),
            next_request_id: AtomicU64::default(),
            auth: Mutex::new(options.auth.clone()),
            reconnect: reconnect_tx,
        });
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (events_tx, events_rx) = mpsc::unbounded();
        let client = Self {
            shared: shared.clone(),
            request_timeout: options.request_timeout,
            sleep: options.sleep.clone(),
            _shutdown: Arc::new(shutdown_tx),
        };
        (
            client,
            events_rx,
            drive(shared, options, events_tx, shutdown_rx, reconnect_rx),
        )
    }

    /// Replaces the credentials sent from the next connection attempt on.
    pub fn set_auth(&self, auth: Option<HelloAuth>) {
        *self.shared.auth.lock() = auth;
    }

    /// Ends the wait before the next connection attempt: the backoff after a lost connection,
    /// or the indefinite wait after the daemon rejected the credentials.
    pub fn reconnect_now(&self) {
        self.shared.reconnect.unbounded_send(()).ok();
    }

    /// The current connection's `server_info`, or `None` while disconnected.
    pub fn server_info(&self) -> Option<Arc<ServerInfo>> {
        self.shared
            .connection
            .lock()
            .as_ref()
            .map(|connection| connection.server_info.clone())
    }

    pub fn is_connected(&self) -> bool {
        self.shared.connection.lock().is_some()
    }

    pub async fn request<R: Request>(&self, request: &R) -> Result<R::Response> {
        let body = serde_json::to_value(request)
            .with_context(|| format!("failed to encode {}", R::TYPE))?;
        let response = self
            .send_request(R::TYPE, body, None, None, R::TIMEOUT)
            .await?;
        if response.message_type != R::RESPONSE_TYPE {
            bail!(
                "unexpected response to {}: expected {}, got {}",
                R::TYPE,
                R::RESPONSE_TYPE,
                response.message_type
            );
        }
        serde_json::from_value(response.payload)
            .with_context(|| format!("failed to decode {}", R::RESPONSE_TYPE))
    }

    /// Attaches to a terminal's stream. Its frames (starting with the restore of the screen the
    /// daemon sends after the response) arrive on the returned receiver instead of as
    /// [`Event::Terminal`]. The receiver closes when the connection is lost.
    pub async fn subscribe_terminal(
        &self,
        request: &SubscribeTerminal,
    ) -> Result<(
        SubscribeTerminalResponse,
        mpsc::UnboundedReceiver<TerminalFrame>,
    )> {
        let body = serde_json::to_value(request)
            .with_context(|| format!("failed to encode {}", SubscribeTerminal::TYPE))?;
        let (frames_tx, frames_rx) = mpsc::unbounded();
        let response = self
            .send_request(SubscribeTerminal::TYPE, body, Some(frames_tx), None, None)
            .await?;
        let response: SubscribeTerminalResponse = serde_json::from_value(response.payload)
            .with_context(|| format!("failed to decode {}", SubscribeTerminal::RESPONSE_TYPE))?;
        if let Some(error) = &response.error {
            bail!(
                "could not attach to terminal {}: {error}",
                request.terminal_id
            );
        }
        Ok((response, frames_rx))
    }

    /// Sends any session request and returns the correlated response untyped. For messages
    /// this crate does not model yet.
    pub async fn request_raw(&self, message_type: &str, body: Value) -> Result<RawMessage> {
        self.send_request(message_type, body, None, None, None)
            .await
    }

    /// Sends a request the daemon answers with updates before its response, and returns the
    /// response with the updates. An update that does not decode is logged and left out.
    pub async fn request_with_updates<R: StreamingRequest>(
        &self,
        request: &R,
    ) -> Result<(R::Response, Vec<R::Update>)> {
        let body = serde_json::to_value(request)
            .with_context(|| format!("failed to encode {}", R::TYPE))?;
        let (payloads_tx, mut payloads_rx) = mpsc::unbounded();
        let route = UpdateRoute {
            message_type: R::UPDATE_TYPE,
            payloads: payloads_tx,
        };
        let response = self
            .send_request(R::TYPE, body, None, Some(route), R::TIMEOUT)
            .await?;
        if response.message_type != R::RESPONSE_TYPE {
            bail!(
                "unexpected response to {}: expected {}, got {}",
                R::TYPE,
                R::RESPONSE_TYPE,
                response.message_type
            );
        }
        let response = serde_json::from_value(response.payload)
            .with_context(|| format!("failed to decode {}", R::RESPONSE_TYPE))?;
        // The updates came before the response, on the same connection.
        let mut updates = Vec::new();
        while let Ok(payload) = payloads_rx.try_recv() {
            match serde_json::from_value(payload) {
                Ok(update) => updates.push(update),
                Err(error) => log::warn!("failed to decode {}: {error}", R::UPDATE_TYPE),
            }
        }
        Ok((response, updates))
    }

    async fn send_request(
        &self,
        message_type: &str,
        body: Value,
        terminal_route: Option<mpsc::UnboundedSender<TerminalFrame>>,
        update_route: Option<UpdateRoute>,
        timeout: Option<Duration>,
    ) -> Result<RawMessage> {
        let Value::Object(mut message) = body else {
            bail!("{message_type} body must be a JSON object");
        };
        let request_id = format!(
            "{}-{}",
            self.shared.request_id_prefix,
            self.shared.next_request_id.fetch_add(1, Ordering::Relaxed)
        );
        message.insert("type".into(), message_type.into());
        message.insert("requestId".into(), request_id.clone().into());
        let frame = session_frame(Value::Object(message))?;

        let (response_tx, response_rx) = oneshot::channel();
        {
            let connection = self.shared.connection.lock();
            let connection = connection
                .as_ref()
                .context("not connected to the Paseo daemon")?;
            self.shared.pending.lock().insert(
                request_id.clone(),
                PendingResponse {
                    response: response_tx,
                    terminal_route,
                    update_route,
                },
            );
            if connection.outgoing.unbounded_send(frame).is_err() {
                self.shared.pending.lock().remove(&request_id);
                bail!("connection to the Paseo daemon closed");
            }
        }

        let pending = PendingGuard {
            shared: &self.shared,
            request_id: &request_id,
        };
        let request_timeout = timeout.unwrap_or(self.request_timeout);
        let mut timeout = (self.sleep)(request_timeout).fuse();
        let mut response_rx = response_rx.fuse();
        let result = select_biased! {
            response = response_rx => response
                .map_err(|_| anyhow!("{message_type} was dropped by the client"))
                .and_then(|response| response),
            _ = timeout => Err(anyhow!(
                "{message_type} timed out after {request_timeout:?}"
            )),
        };
        drop(pending);
        result
    }

    /// Sends a session message that has no response.
    pub fn notify<N: Notification>(&self, notification: &N) -> Result<()> {
        let mut body = serde_json::to_value(notification)
            .with_context(|| format!("failed to encode {}", N::TYPE))?;
        let Value::Object(message) = &mut body else {
            bail!("{} must encode to a JSON object", N::TYPE);
        };
        message.insert("type".into(), N::TYPE.into());
        self.shared.send(session_frame(body)?)
    }

    pub fn send_terminal_frame(&self, frame: &TerminalFrame) -> Result<()> {
        self.shared.send(Message::binary(frame.encode()))
    }
}

/// Removes the pending entry when a request finishes, times out or is cancelled.
struct PendingGuard<'a> {
    shared: &'a Shared,
    request_id: &'a str,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.shared.pending.lock().remove(self.request_id);
    }
}

fn session_frame(message: Value) -> Result<Message> {
    let frame = serde_json::json!({ "type": "session", "message": message });
    Ok(Message::text(serde_json::to_string(&frame)?))
}

async fn drive(
    shared: Arc<Shared>,
    options: ConnectOptions,
    events: mpsc::UnboundedSender<Event>,
    shutdown: oneshot::Receiver<()>,
    mut reconnect: mpsc::UnboundedReceiver<()>,
) {
    // `oneshot::Receiver` reports itself terminated once its sender is dropped, which makes
    // `select!` skip it; `Fuse` only terminates after the future actually completed.
    let mut shutdown = futures::FutureExt::fuse(shutdown);
    let mut attempt = 0;
    loop {
        let error = {
            let connection = run_connection(&shared, &options, &events, &mut attempt).fuse();
            futures::pin_mut!(connection);
            select_biased! {
                _ = shutdown => {
                    shared.disconnected("client dropped");
                    return;
                }
                result = connection => match result {
                    Ok(()) => anyhow!("connection closed"),
                    Err(error) => error,
                },
            }
        };
        let reason = format!("{error:#}");
        log::info!("Paseo daemon {}: {reason}", options.endpoint);
        shared.disconnected(&reason);
        events.unbounded_send(Event::Disconnected { reason }).ok();

        let rejection = error
            .downcast_ref::<HelloRejected>()
            .map(|rejected| rejected.0)
            .filter(|reason| {
                matches!(
                    reason,
                    HelloRejectionReason::PasswordRequired
                        | HelloRejectionReason::IncorrectPassword
                )
            });
        if let Some(reason) = rejection {
            // The same credentials would be refused again.
            events.unbounded_send(Event::Rejected(reason)).ok();
            attempt = 0;
            select_biased! {
                _ = shutdown => return,
                _ = reconnect.next() => {}
            }
            continue;
        }

        let delay = options.reconnect_delay(attempt);
        attempt = attempt.saturating_add(1);
        let mut delay = (options.sleep)(delay).fuse();
        select_biased! {
            _ = shutdown => return,
            _ = delay => {}
            _ = reconnect.next() => {}
        }
    }
}

trait Socket: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Socket for T {}

async fn open_socket(endpoint: &Endpoint) -> Result<Box<dyn Socket>> {
    Ok(match endpoint {
        Endpoint::Tcp { host, port, .. } => Box::new(
            smol::net::TcpStream::connect((host.as_str(), *port))
                .await
                .with_context(|| format!("failed to connect to {endpoint}"))?,
        ),
        Endpoint::Unix(path) => Box::new(
            smol::net::unix::UnixStream::connect(path)
                .await
                .with_context(|| format!("failed to connect to {endpoint}"))?,
        ),
    })
}

/// Returns only when the connection ends; the caller treats every return as a disconnect.
async fn run_connection(
    shared: &Shared,
    options: &ConnectOptions,
    events: &mpsc::UnboundedSender<Event>,
    attempt: &mut u32,
) -> Result<()> {
    let socket = open_socket(&options.endpoint).await?;
    let request = options.endpoint.websocket_url().into_client_request()?;
    let (websocket, _) = async_tungstenite::client_async(request, socket)
        .await
        .context("WebSocket handshake failed")?;
    let (mut sink, mut stream) = websocket.split();

    let auth = shared.auth.lock().clone();
    let hello = Hello {
        message_type: "hello",
        client_id: &options.client_id,
        client_type: options.client_type,
        protocol_version: PROTOCOL_VERSION,
        auth: auth.as_ref(),
        app_version: options.app_version.as_deref(),
        capabilities: &options.capabilities,
    };
    sink.send(Message::text(serde_json::to_string(&hello)?))
        .await?;

    let mut timeout = (options.sleep)(HELLO_TIMEOUT).fuse();
    let server_info = select_biased! {
        server_info = wait_for_server_info(&mut stream).fuse() => server_info?,
        _ = timeout => bail!("no server_info within {HELLO_TIMEOUT:?} of hello"),
    };

    let mut chunks = ChunkAssembler::default();
    chunks.set_enabled(
        server_info.has_feature("binaryChunkingV1")
            && options.capabilities.get(capabilities::BINARY_CHUNKING_V1) == Some(&true),
    );
    *attempt = 0;
    let server_info = Arc::new(server_info);
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded();
    *shared.connection.lock() = Some(Connection {
        outgoing: outgoing_tx,
        server_info: server_info.clone(),
    });
    events.unbounded_send(Event::Connected(server_info)).ok();

    let mut ping = (options.sleep)(options.ping_interval).fuse();
    let mut last_received = Instant::now();
    loop {
        select_biased! {
            message = stream.next().fuse() => {
                let message = message
                    .context("the daemon closed the connection")?
                    .context("failed to read from the daemon")?;
                last_received = Instant::now();
                let frame = match message {
                    Message::Text(text) => Frame::Text(text.to_string()),
                    Message::Binary(bytes) => Frame::Binary(bytes.to_vec()),
                    Message::Close(close) => bail!("the daemon closed the connection: {close:?}"),
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                };
                if let Some(frame) = chunks.receive(frame)? {
                    handle_frame(shared, events, frame);
                }
            }
            message = outgoing_rx.next() => {
                if let Some(message) = message {
                    sink.send(message).await.context("failed to write to the daemon")?;
                }
            }
            _ = ping => {
                ping = (options.sleep)(options.ping_interval).fuse();
                // Any inbound frame proves liveness; the daemon answers pings with pongs.
                if last_received.elapsed() > options.ping_interval * 3 {
                    bail!("no message from the daemon for {:?}", last_received.elapsed());
                }
                sink.send(Message::text(r#"{"type":"ping"}"#))
                    .await
                    .context("failed to write to the daemon")?;
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Envelope {
    #[serde(rename = "session")]
    Session { message: RawMessage },
    #[serde(rename = "pong")]
    Pong,
    #[serde(rename = "hello.rejected")]
    HelloRejected { reason: HelloRejectionReason },
    #[serde(other)]
    Unknown,
}

async fn wait_for_server_info<S>(stream: &mut S) -> Result<ServerInfo>
where
    S: futures::Stream<Item = async_tungstenite::tungstenite::Result<Message>> + Unpin,
{
    while let Some(message) = stream.next().await {
        let Message::Text(text) = message.context("failed to read from the daemon")? else {
            continue;
        };
        match serde_json::from_str::<Envelope>(&text) {
            Ok(Envelope::HelloRejected { reason }) => return Err(HelloRejected(reason).into()),
            Ok(Envelope::Session { message })
                if message.message_type == "status"
                    && message.payload.get("status") == Some(&Value::from("server_info")) =>
            {
                return serde_json::from_value(message.payload)
                    .context("failed to decode server_info");
            }
            Ok(_) => log::debug!("ignoring Paseo message before server_info"),
            Err(error) => log::warn!("invalid Paseo message before server_info: {error}"),
        }
    }
    bail!("the daemon closed the connection before server_info")
}

fn handle_frame(shared: &Shared, events: &mpsc::UnboundedSender<Event>, frame: Frame) {
    match frame {
        Frame::Text(text) => handle_text(shared, events, &text),
        Frame::Binary(bytes) => match TerminalFrame::decode(&bytes) {
            Some(frame) => {
                let slot = frame.slot;
                let mut routes = shared.terminal_routes.lock();
                if let Some(route) = routes.get(&slot) {
                    // A closed route means the stream's reader is gone; nobody else wants it.
                    if route.unbounded_send(frame).is_err() {
                        routes.remove(&slot);
                    }
                    return;
                }
                drop(routes);
                events.unbounded_send(Event::Terminal(frame)).ok();
            }
            None => log::debug!(
                "ignoring Paseo binary frame with opcode {:?}",
                bytes.first()
            ),
        },
    }
}

fn handle_text(shared: &Shared, events: &mpsc::UnboundedSender<Event>, text: &str) {
    let message = match serde_json::from_str::<Envelope>(text) {
        Ok(Envelope::Session { message }) => message,
        Ok(Envelope::Pong) => return,
        Ok(Envelope::HelloRejected { reason }) => {
            log::warn!("Paseo daemon rejected hello after accepting it: {reason:?}");
            return;
        }
        Ok(Envelope::Unknown) => {
            log::debug!("ignoring unknown Paseo envelope");
            return;
        }
        Err(error) => {
            log::warn!("invalid Paseo message: {error}");
            return;
        }
    };

    if let Some(request_id) = message.payload.get("requestId").and_then(Value::as_str) {
        let pending = shared.pending.lock();
        if let Some(route) = pending
            .get(request_id)
            .and_then(|pending| pending.update_route.as_ref())
            && route.message_type == message.message_type
        {
            route.payloads.unbounded_send(message.payload.clone()).ok();
            return;
        }
    }

    // `agent_permission_resolved` also has a `requestId`, but it names a permission request and
    // never matches the client's own `pazedo-` ids.
    let pending = message
        .payload
        .get("requestId")
        .and_then(Value::as_str)
        .and_then(|request_id| shared.pending.lock().remove(request_id));
    if let Some(pending) = pending {
        if let Some(route) = pending.terminal_route
            && message.message_type == SubscribeTerminal::RESPONSE_TYPE
            && let Some(slot) = message.payload.get("slot").and_then(Value::as_u64)
            && let Ok(slot) = u8::try_from(slot)
        {
            shared.terminal_routes.lock().insert(slot, route);
        }
        let result = if message.message_type == "rpc_error" {
            match serde_json::from_value::<RpcErrorPayload>(message.payload) {
                Ok(error) => Err(RpcError {
                    request_type: error.request_type,
                    code: error.code,
                    message: error.error,
                }
                .into()),
                Err(error) => Err(anyhow!("failed to decode rpc_error: {error}")),
            }
        } else {
            Ok(message)
        };
        pending.response.send(result).ok();
        return;
    }

    events
        .unbounded_send(Event::Message(ServerMessage::decode(
            message.message_type,
            message.payload,
        )))
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_endpoints() {
        assert_eq!(
            Endpoint::parse("127.0.0.1:6868").unwrap(),
            Endpoint::Tcp {
                host: "127.0.0.1".into(),
                port: 6868,
                path: "/ws".into()
            }
        );
        assert_eq!(
            Endpoint::parse("ws://mini.tail:7000/ws")
                .unwrap()
                .to_string(),
            "ws://mini.tail:7000/ws"
        );
        assert_eq!(
            Endpoint::parse("[::1]:6767").unwrap().to_string(),
            "ws://[::1]:6767/ws"
        );
        assert_eq!(
            Endpoint::parse("localhost").unwrap().to_string(),
            "ws://localhost:6767/ws"
        );
        assert_eq!(
            Endpoint::parse("unix:///tmp/paseo.sock").unwrap(),
            Endpoint::Unix("/tmp/paseo.sock".into())
        );
        assert_eq!(
            Endpoint::parse("/tmp/paseo.sock").unwrap(),
            Endpoint::Unix("/tmp/paseo.sock".into())
        );
        assert!(Endpoint::parse("wss://relay.paseo.sh").is_err());
        assert!(Endpoint::parse("host:notaport").is_err());
    }

    #[test]
    fn reconnect_delay_matches_paseo_backoff() {
        let options = ConnectOptions::new(
            Endpoint::parse("localhost").unwrap(),
            "test",
            fake_daemon::smol_sleep(),
        );
        assert_eq!(options.reconnect_delay(0), Duration::from_millis(1500));
        assert_eq!(options.reconnect_delay(2), Duration::from_millis(6000));
        assert_eq!(options.reconnect_delay(10), Duration::from_secs(30));
        assert_eq!(options.reconnect_delay(u32::MAX), Duration::from_secs(30));
    }
}
