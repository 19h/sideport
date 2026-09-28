//! Local IPC between Sideport processes, recovered from Sideloadly's `gui/ipc` package.
//!
//! The desktop app serves HTTP/1.1 on port [`DEFAULT_PORT`] of the loopback interface:
//!
//! - `GET /raise?fn=<file>`: the app comes forward and opens `fn` when present; the reply is
//!   `success`. An instance that cannot bind the port sends this and exits on `success`.
//! - `GET /restart?e=<message>`: 200; the app exits so another instance can take over.
//! - `GET /enqueue?id=<installation>`: the serving process queues and runs that refresh. Replies
//!   `Task enqueued` or (500) `Task enqueue fail`, each followed by `OK`, or `ERROR <n> <reason>`
//!   for a malformed id.
//! - `GET /tokens?user_token=<token>`: the sign-in return page. The token goes to the one sign-in
//!   waiting for it; without one the reply is 500 with the recovered apology.
//! - `GET /poll?v=<version>`: `Version mismatch` for another version; otherwise the next message the
//!   app left, or `bye` when the server stops. Plain text with `X-Content-Type-Options: nosniff`.
//!
//! Deviation: the recovered server answered any local request, so a web page could make the app
//! open a file, queue refreshes or exit. Sideport binds 127.0.0.1 only, requires `Host` to name the
//! loopback port, and requires the [`TOKEN_HEADER`] header with the secret in `<data>/ipc-token`
//! (mode 0600) on every route except `/tokens`, which a browser reaches by redirect and which only
//! completes a sign-in the app started. Browsers cannot attach that header to a cross-origin request
//! without a preflight, and the server refuses preflights.

use crate::error::{EngineError, Result};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

/// The recovered IPC port.
pub const DEFAULT_PORT: u16 = 28811;

/// Header carrying the data directory's IPC secret.
pub const TOKEN_HEADER: &str = "X-Sideport-Token";

pub(crate) const TOKEN_FILE: &str = "ipc-token";

/// How long a client waits to connect and for a short reply.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound for a reply body read by the client.
const MAX_REPLY: u64 = 64 * 1024;

/// Requests another process made of the desktop app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcEvent {
    /// Come forward; open `file` when present.
    Raise { file: Option<String> },
    /// Exit so another instance can take over (`message` explains why, when given).
    Restart { message: Option<String> },
    /// An installation was queued for refresh through `/enqueue`.
    Enqueued { installation_id: i64 },
}

/// Outcome of a `/poll` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollReply {
    Message(String),
    VersionMismatch,
    /// The server stopped.
    Bye,
}

/// The version sent as `v`, compared without pre-release or build suffixes.
pub(crate) fn base_version(version: &str) -> &str {
    version.split(['-', '+']).next().unwrap_or(version)
}

/// Read the data directory's IPC secret, creating it when `create` is set.
pub(crate) fn token(data_dir: &Path, create: bool) -> Result<String> {
    let path = data_dir.join(TOKEN_FILE);

    match std::fs::read_to_string(&path) {
        Ok(token) if !token.trim().is_empty() => return Ok(token.trim().to_owned()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(EngineError::Storage(error.to_string())),
    }

    if !create {
        return Err(EngineError::Ipc("no Sideport app has served this data directory yet".into()));
    }

    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    write_private(&path, token.as_bytes())?;

    Ok(token)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let storage = |error: std::io::Error| EngineError::Storage(error.to_string());
    let directory = path.parent().ok_or_else(|| EngineError::Storage("invalid token path".into()))?;

    std::fs::create_dir_all(directory).map_err(storage)?;

    let mut temporary = tempfile::NamedTempFile::new_in(directory).map_err(storage)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary.as_file().set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(storage)?;
    }

    temporary.write_all(bytes).map_err(storage)?;
    temporary.persist(path).map_err(|error| storage(error.error))?;

    Ok(())
}

/// A blocking client for another process's IPC server on this machine.
#[derive(Debug, Clone)]
pub struct IpcClient {
    port: u16,
    token: String,
}

impl IpcClient {
    /// A client for the app serving `data_dir` on `port`.
    pub fn new(data_dir: &Path, port: u16) -> Result<Self> {
        Ok(Self { port, token: token(data_dir, false)? })
    }

    /// Ask the running app to come forward (and open `file`); `true` when it agreed.
    pub fn raise(&self, file: Option<&str>) -> Result<bool> {
        let query = file.map(|file| encode(&[("fn", file)])).unwrap_or_default();
        let (_, body) = self.get("/raise", &query, Some(CLIENT_TIMEOUT))?;

        Ok(body == "success")
    }

    /// Ask the running app to exit so this process can take over.
    pub fn restart(&self, message: Option<&str>) -> Result<()> {
        let query = encode(&[("e", message.unwrap_or_default())]);
        let (status, _) = self.get("/restart", &query, Some(CLIENT_TIMEOUT))?;

        if status != 200 {
            return Err(EngineError::Ipc(format!("restart refused with status {status}")));
        }

        Ok(())
    }

    /// Queue an installation's refresh in the running app.
    pub fn enqueue(&self, installation_id: i64) -> Result<()> {
        let query = encode(&[("id", &installation_id.to_string())]);
        let (status, body) = self.get("/enqueue", &query, Some(CLIENT_TIMEOUT))?;

        if status != 200 || !body.starts_with("Task enqueued") {
            return Err(EngineError::Ipc(format!("the app did not queue the refresh: {body}")));
        }

        Ok(())
    }

    /// Wait for the next message the running app leaves (no timeout).
    pub fn poll(&self) -> Result<PollReply> {
        let query = encode(&[("v", env!("CARGO_PKG_VERSION"))]);
        let (_, body) = self.get("/poll", &query, None)?;

        Ok(match body.as_str() {
            "Version mismatch" => PollReply::VersionMismatch,
            "bye" => PollReply::Bye,
            _ => PollReply::Message(body),
        })
    }

    fn get(&self, path: &str, query: &str, timeout: Option<Duration>) -> Result<(u16, String)> {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, self.port));
        let unreachable = |error: std::io::Error| EngineError::Ipc(format!("Sideport is not running: {error}"));
        let failed = |error: std::io::Error| EngineError::Ipc(error.to_string());

        let mut stream = TcpStream::connect_timeout(&address, CLIENT_TIMEOUT).map_err(unreachable)?;
        stream.set_read_timeout(timeout).map_err(failed)?;
        stream.set_write_timeout(Some(CLIENT_TIMEOUT)).map_err(failed)?;

        let target = if query.is_empty() { path.to_owned() } else { format!("{path}?{query}") };
        let request = format!(
            "GET {target} HTTP/1.1\r\nHost: localhost:{}\r\n{TOKEN_HEADER}: {}\r\nConnection: close\r\n\r\n",
            self.port, self.token
        );
        stream.write_all(request.as_bytes()).map_err(failed)?;

        let mut reply = Vec::new();
        stream.take(MAX_REPLY).read_to_end(&mut reply).map_err(failed)?;

        parse_reply(&reply)
    }
}

fn parse_reply(reply: &[u8]) -> Result<(u16, String)> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut headers);

    let head = match response.parse(reply) {
        Ok(httparse::Status::Complete(length)) => length,
        Ok(httparse::Status::Partial) => return Err(EngineError::Ipc("incomplete reply".into())),
        Err(error) => return Err(EngineError::Ipc(format!("invalid reply: {error}"))),
    };

    let status = response.code.unwrap_or_default();
    let body = String::from_utf8_lossy(&reply[head..]).into_owned();

    Ok((status, body))
}

pub(crate) fn encode(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_without_suffixes_and_tokens_are_private_and_stable() {
        assert_eq!(base_version("0.60.0-beta.2"), "0.60.0");
        assert_eq!(base_version("0.60.0+build"), "0.60.0");

        let directory = tempfile::tempdir().expect("tempdir");
        assert!(matches!(token(directory.path(), false), Err(EngineError::Ipc(_))));

        let created = token(directory.path(), true).expect("create");
        assert_eq!(created.len(), 64);
        assert_eq!(token(directory.path(), false).expect("read"), created);
        assert_eq!(token(directory.path(), true).expect("reuse"), created);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(directory.path().join(TOKEN_FILE)).expect("metadata").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
