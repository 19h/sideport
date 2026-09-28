//! The desktop app's local IPC server; the protocol is described in [`crate::ipc`].

use super::{Inner, refresh};
use crate::error::{EngineError, Result};
use crate::ipc::{IpcEvent, TOKEN_HEADER, base_version};
use chrono::Utc;
use futures::channel::oneshot;
use parking_lot::Mutex;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// Largest accepted request head (request line and headers).
const MAX_REQUEST_HEAD: usize = 8 * 1024;

/// How long a client may take to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Messages kept for `/poll`; the recovered `LeaveMessage` drops the oldest when ten wait.
const MESSAGE_BACKLOG: usize = 10;

/// The recovered return page, naming this app.
const SIGNED_IN_PAGE: &str = concat!(
    "You're successfully logged in, Sideport will now open so you can safely close this tab.",
    "<script>window.close()</script>",
);
const SIGN_IN_ERROR: &str = "You're successfully logged in, but there's internal error... o_O";

/// Messages waiting for `/poll` and the sign-in waiting for `/tokens`.
#[derive(Debug)]
pub(crate) struct Mailbox {
    sender: async_channel::Sender<String>,
    receiver: async_channel::Receiver<String>,
    sign_in: Mutex<Option<oneshot::Sender<String>>>,
}

impl Mailbox {
    pub(crate) fn new() -> Self {
        let (sender, receiver) = async_channel::bounded(MESSAGE_BACKLOG);

        Self { sender, receiver, sign_in: Mutex::new(None) }
    }

    pub(crate) fn leave(&self, message: String) {
        let _ = self.sender.force_send(message);
    }

    /// Wait for the next `/tokens` request; a later call replaces this waiter.
    pub(crate) fn expect_sign_in(&self) -> oneshot::Receiver<String> {
        let (sender, receiver) = oneshot::channel();
        *self.sign_in.lock() = Some(sender);

        receiver
    }
}

/// A running IPC server; dropping it stops the server and answers pending polls with `bye`.
#[derive(Debug)]
pub struct IpcServer {
    port: u16,
    stop: CancellationToken,
}

impl IpcServer {
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct Server {
    inner: Arc<Inner>,
    port: u16,
    token: String,
    stop: CancellationToken,
}

/// Bind the loopback port and serve until the returned handle is dropped.
pub(super) fn serve(inner: &Arc<Inner>, port: u16) -> Result<IpcServer> {
    let token = crate::ipc::token(&inner.data_dir, true)?;

    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = std::net::TcpListener::bind(address).map_err(|error| match error.kind() {
        std::io::ErrorKind::AddrInUse => EngineError::Ipc(format!("port {port} is already in use")),
        _ => EngineError::Ipc(error.to_string()),
    })?;
    listener.set_nonblocking(true).map_err(|error| EngineError::Ipc(error.to_string()))?;

    let port = listener.local_addr().map_err(|error| EngineError::Ipc(error.to_string()))?.port();
    let listener = {
        let _runtime = inner.runtime.handle.enter();
        TcpListener::from_std(listener).map_err(|error| EngineError::Ipc(error.to_string()))?
    };

    let stop = CancellationToken::new();
    let server = Arc::new(Server { inner: inner.clone(), port, token, stop: stop.clone() });

    inner.runtime.handle.spawn(async move {
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                () = server.stop.cancelled() => return,
            };

            if let Ok((stream, _)) = accepted {
                tokio::spawn(server.clone().connection(stream));
            }
        }
    });

    Ok(IpcServer { port, stop })
}

/// The parts of a request the routes use.
#[derive(Debug, Default)]
struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    host: Option<String>,
    token: Option<String>,
}

impl Request {
    fn parameter(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }
}

struct Reply {
    status: u16,
    content_type: &'static str,
    nosniff: bool,
    body: String,
}

impl Reply {
    fn text(status: u16, body: impl Into<String>) -> Self {
        Self { status, content_type: "text/plain; charset=utf-8", nosniff: false, body: body.into() }
    }

    fn encode(&self) -> Vec<u8> {
        let reason = match self.status {
            200 => "OK",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            _ => "Internal Server Error",
        };

        let mut head = format!("HTTP/1.1 {} {reason}\r\n", self.status);
        head.push_str(&format!("Content-Type: {}\r\n", self.content_type));
        head.push_str(&format!("Content-Length: {}\r\n", self.body.len()));

        if self.nosniff {
            head.push_str("X-Content-Type-Options: nosniff\r\n");
        }

        head.push_str("Connection: close\r\n\r\n");

        [head.into_bytes(), self.body.clone().into_bytes()].concat()
    }
}

impl Server {
    async fn connection(self: Arc<Self>, mut stream: TcpStream) {
        let Ok(Some(request)) = tokio::time::timeout(REQUEST_TIMEOUT, read_request(&mut stream)).await else {
            return;
        };

        let reply = match self.refusal(&request) {
            Some(refusal) => Some(refusal),
            None => self.route(&request, &mut stream).await,
        };

        if let Some(reply) = reply {
            let _ = stream.write_all(&reply.encode()).await;
        }

        let _ = stream.shutdown().await;
    }

    /// Requests the recovered server accepted but Sideport refuses (see [`crate::ipc`]).
    fn refusal(&self, request: &Request) -> Option<Reply> {
        if request.method != "GET" {
            return Some(Reply::text(405, "method not allowed"));
        }

        let loopback = [format!("localhost:{}", self.port), format!("127.0.0.1:{}", self.port)];

        if !request.host.as_ref().is_some_and(|host| loopback.contains(&host.to_ascii_lowercase())) {
            return Some(Reply::text(403, "forbidden host"));
        }

        let authorized = request.token.as_deref().is_some_and(|token| constant_time_eq(token, &self.token));

        if request.path != "/tokens" && !authorized {
            return Some(Reply::text(403, "missing or wrong IPC token"));
        }

        None
    }

    /// Answer a request; `None` when a poller went away before a message arrived.
    async fn route(&self, request: &Request, stream: &mut TcpStream) -> Option<Reply> {
        let reply = match request.path.as_str() {
            "/raise" => {
                let file = request.parameter("fn").filter(|file| !file.is_empty()).map(str::to_owned);
                self.inner.publish_ipc(&IpcEvent::Raise { file });

                Reply::text(200, "success")
            }

            "/restart" => {
                let message = request.parameter("e").filter(|message| !message.is_empty()).map(str::to_owned);
                self.inner.publish_ipc(&IpcEvent::Restart { message });

                Reply::text(200, "")
            }

            "/enqueue" => self.enqueue(request.parameter("id").unwrap_or_default()),

            "/tokens" => self.sign_in(request.parameter("user_token").unwrap_or_default()),

            "/poll" => return self.poll(request.parameter("v").unwrap_or_default(), stream).await,

            _ => Reply::text(404, "404 page not found"),
        };

        Some(reply)
    }

    /// Recovered replies: `Task enqueued` or `Task enqueue fail`, then `OK` in the same body.
    fn enqueue(&self, id: &str) -> Reply {
        let installation_id = match scan_integer(id) {
            Ok(installation_id) => installation_id,
            Err(reason) => return Reply::text(200, format!("ERROR 0 {reason}")),
        };

        match self.queue(installation_id) {
            Ok(()) => {
                self.inner.publish_ipc(&IpcEvent::Enqueued { installation_id });

                Reply::text(200, "Task enqueuedOK")
            }

            Err(error) => {
                tracing::warn!("IPC enqueue of installation {installation_id} failed: {error}");

                Reply::text(500, "Task enqueue failOK")
            }
        }
    }

    fn queue(&self, installation_id: i64) -> Result<()> {
        if self.inner.demo.is_some() {
            return Err(EngineError::Unsupported("the demo backend has no refresh queue".into()));
        }

        if self.inner.store.installation(installation_id)?.is_none() {
            return Err(EngineError::Storage(format!("installation {installation_id} does not exist")));
        }

        let token = uuid::Uuid::new_v4().to_string();
        self.inner.store.enqueue_refresh(installation_id, &token, Utc::now())?;

        let inner = self.inner.clone();
        self.inner.runtime.handle.spawn(async move {
            if let Err(error) = refresh::run_queue(&inner).await {
                tracing::warn!("refresh queue failed: {error}");
            }
        });

        Ok(())
    }

    fn sign_in(&self, user_token: &str) -> Reply {
        let waiter = if user_token.is_empty() { None } else { self.inner.ipc.sign_in.lock().take() };

        match waiter.map(|waiter| waiter.send(user_token.to_owned())) {
            Some(Ok(())) => {
                Reply { status: 200, content_type: "text/html", nosniff: false, body: SIGNED_IN_PAGE.into() }
            }
            _ => Reply::text(500, SIGN_IN_ERROR),
        }
    }

    /// Wait for a message; `bye` when the server stops, nothing when the client disconnects.
    async fn poll(&self, version: &str, stream: &mut TcpStream) -> Option<Reply> {
        let plain = |body: &str| Reply { status: 200, content_type: "text/plain", nosniff: true, body: body.into() };

        if base_version(version) != base_version(env!("CARGO_PKG_VERSION")) {
            return Some(plain("Version mismatch"));
        }

        let mut probe = [0u8; 1];

        tokio::select! {
            message = self.inner.ipc.receiver.recv() => message.ok().map(|message| plain(&message)),
            () = self.stop.cancelled() => Some(plain("bye")),
            _ = stream.read(&mut probe) => None,
        }
    }
}

/// Read and parse one request head; `None` for malformed or oversized requests.
async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    loop {
        let read = stream.read(&mut chunk).await.ok()?;

        if read == 0 {
            return None;
        }

        buffer.extend_from_slice(&chunk[..read]);

        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }

        if buffer.len() > MAX_REQUEST_HEAD {
            return None;
        }
    }

    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut parsed = httparse::Request::new(&mut headers);

    if !matches!(parsed.parse(&buffer), Ok(httparse::Status::Complete(_))) {
        return None;
    }

    let target = parsed.path?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let header = |name: &str| {
        parsed
            .headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .and_then(|header| std::str::from_utf8(header.value).ok())
            .map(str::to_owned)
    };

    Some(Request {
        method: parsed.method?.to_owned(),
        path: path.to_owned(),
        query: url::form_urlencoded::parse(query.as_bytes()).into_owned().collect(),
        host: header("host"),
        token: header(TOKEN_HEADER),
    })
}

/// Go's `fmt.Fscanf("%d")`: optional leading spaces and sign, then at least one digit; trailing
/// characters are ignored.
fn scan_integer(text: &str) -> std::result::Result<i64, &'static str> {
    let trimmed = text.trim_start();

    if trimmed.is_empty() {
        return Err("unexpected EOF");
    }

    let sign_length = usize::from(trimmed.starts_with(['+', '-']));
    let digits = trimmed[sign_length..].bytes().take_while(u8::is_ascii_digit).count();

    if digits == 0 {
        return Err("expected integer");
    }

    trimmed[..sign_length + digits].parse().map_err(|_| "value out of range")
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    left.len() == right.len() && left.bytes().zip(right.bytes()).fold(0u8, |acc, (l, r)| acc | (l ^ r)) == 0
}

#[cfg(test)]
mod tests;
