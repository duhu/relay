//! The unix socket between the short-lived `relay` CLI and the resident app.
//!
//! One JSON request line in, one JSON response line out, then the connection
//! closes (spec §7). The socket carries no authentication: it lives in the
//! user's own Application Support directory and is reachable only by them.

use std::future::Future;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream as SyncUnixStream;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// How long the CLI waits for the tiny request line to be written. Reading
/// the answer uses a caller-supplied timeout instead (see `call`), because a
/// real switch can legitimately take much longer than a status lookup.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a connected client has to send its request line. A request is one
/// short line the CLI already has in hand, so anything slower than this is a
/// client that died mid-write or one that is not going to ask anything.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest request line the server will read. Every request is a handful
/// of bytes; the cap is what keeps a stray writer from growing the buffer
/// until the app runs out of memory.
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Switch {
        /// The HID++ host slot, 0-based, exactly as `config.json` declares it.
        target: u8,
        #[serde(default)]
        dry_run: bool,
    },
    OpenSettings,
    ReloadConfig,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Ok { data: serde_json::Value },
    Error { message: String },
}

impl Response {
    pub fn ok(data: serde_json::Value) -> Self {
        Response::Ok { data }
    }

    pub fn error(message: impl std::fmt::Display) -> Self {
        Response::Error {
            message: message.to_string(),
        }
    }
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// What the resident app does with a request. Cloned per connection.
pub type Handler = Arc<dyn Fn(Request) -> BoxFuture<Response> + Send + Sync>;

/// Accepts connections on `path` until the listener fails.
///
/// A socket file left behind by a crash would make `bind` fail with
/// `EADDRINUSE`, so it is removed first; what actually keeps a second app from
/// starting is the lock file, taken before this is ever called.
pub async fn serve(path: &Path, handler: Handler) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(path);

    let listener = UnixListener::bind(path)?;
    // The protocol has no authentication of its own, so the file mode is what
    // keeps every other account on the machine from talking to the app.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(socket = %path.display(), "IPC server listening");

    loop {
        let (stream, _) = listener.accept().await?;
        let handler = handler.clone();
        tokio::spawn(async move {
            if let Err(err) = answer(stream, handler).await {
                tracing::warn!(error = %err, "an IPC connection failed");
            }
        });
    }
}

/// Reads one request line, answers it and lets the connection close.
///
/// A client that says nothing, or that keeps writing one endless line, is told
/// so and dropped: neither may hold a task (or the app's memory) hostage.
async fn answer(stream: UnixStream, handler: Handler) -> io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    // One byte past the limit, so that hitting it is distinguishable from a
    // line that is exactly as long as a request may be.
    let mut reader = tokio::io::BufReader::new(reader.take(MAX_REQUEST_BYTES + 1));

    let mut line = String::new();
    let response = match tokio::time::timeout(READ_TIMEOUT, reader.read_line(&mut line)).await {
        // A client that connected and left without asking anything.
        Ok(Ok(0)) => return Ok(()),
        Ok(Ok(_)) if line.len() as u64 > MAX_REQUEST_BYTES => Response::error(format!(
            "the request is too long: more than {MAX_REQUEST_BYTES} bytes"
        )),
        Ok(Ok(_)) => match serde_json::from_str::<Request>(&line) {
            Ok(request) => handler(request).await,
            Err(err) => Response::error(format!("cannot read the request: {err}")),
        },
        Ok(Err(err)) => return Err(err),
        Err(_) => Response::error(format!("no request within {}s", READ_TIMEOUT.as_secs())),
    };

    let mut bytes = serde_json::to_vec(&response)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await
}

/// Sends one request and waits for its answer. Blocking, for the CLI.
///
/// `read_timeout` bounds how long this waits for the response line; callers
/// pick it per request, since a real switch can take far longer than a
/// status lookup (spec: DDC retries plus two device calls, ~60s worst case).
pub fn call(path: &Path, request: &Request, read_timeout: Duration) -> io::Result<Response> {
    let mut stream = SyncUnixStream::connect(path)?;
    stream.set_read_timeout(Some(read_timeout))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;

    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.flush()?;

    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    if answer.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the Relay app closed the connection without answering",
        ));
    }
    Ok(serde_json::from_str(&answer)?)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use serde_json::json;

    use super::*;

    /// The read timeout ordinary (non-switch) tests use for `call`.
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    /// Answers every request with the request itself, so a round trip shows
    /// both directions of the wire format.
    fn echo_handler() -> Handler {
        Arc::new(|request| {
            Box::pin(async move {
                match serde_json::to_value(&request) {
                    Ok(value) => Response::ok(value),
                    Err(err) => Response::error(err),
                }
            })
        })
    }

    /// Answers every request after a real (tokio) sleep, long enough that a
    /// short `call` read timeout will not survive it.
    fn slow_handler(delay: Duration) -> Handler {
        Arc::new(move |request| {
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                match serde_json::to_value(&request) {
                    Ok(value) => Response::ok(value),
                    Err(err) => Response::error(err),
                }
            })
        })
    }

    /// Starts `serve` on a runtime that stays alive for the whole test.
    fn start_server(path: &Path) -> tokio::runtime::Runtime {
        start_server_with(path, echo_handler())
    }

    /// Like `start_server`, with a caller-supplied handler.
    fn start_server_with(path: &Path, handler: Handler) -> tokio::runtime::Runtime {
        let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
        let socket = path.to_path_buf();
        runtime.spawn(async move {
            let _ = serve(&socket, handler).await;
        });

        // A stale file at `path` would make an `exists` check pass before the
        // server has replaced it, so wait for something that actually accepts.
        let deadline = Instant::now() + Duration::from_secs(5);
        while SyncUnixStream::connect(path).is_err() {
            assert!(Instant::now() < deadline, "the server never bound {path:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        runtime
    }

    #[test]
    fn call_round_trips_a_request() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let socket = dir.path().join("relay.sock");
        let _server = start_server(&socket);

        let response = call(
            &socket,
            &Request::Switch {
                target: 2,
                dry_run: true,
            },
            TEST_TIMEOUT,
        )
        .expect("the server answers");

        assert_eq!(
            response,
            Response::ok(json!({ "cmd": "switch", "target": 2, "dry_run": true }))
        );
    }

    #[test]
    fn the_server_answers_more_than_one_client() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let socket = dir.path().join("relay.sock");
        let _server = start_server(&socket);

        for _ in 0..3 {
            let response =
                call(&socket, &Request::Status, TEST_TIMEOUT).expect("the server answers");
            assert_eq!(response, Response::ok(json!({ "cmd": "status" })));
        }
    }

    #[test]
    fn serve_replaces_a_socket_left_behind_by_a_crash() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let socket = dir.path().join("relay.sock");
        std::fs::write(&socket, b"stale").expect("the stale file is written");

        let _server = start_server(&socket);

        assert_eq!(
            call(&socket, &Request::ReloadConfig, TEST_TIMEOUT).expect("the server answers"),
            Response::ok(json!({ "cmd": "reload_config" }))
        );
    }

    #[test]
    fn a_line_that_is_not_a_request_comes_back_as_an_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let socket = dir.path().join("relay.sock");
        let _server = start_server(&socket);

        let mut stream = SyncUnixStream::connect(&socket).expect("the socket accepts");
        stream.write_all(b"{\"cmd\":\"fly\"}\n").expect("the write");
        let mut answer = String::new();
        BufReader::new(stream)
            .read_line(&mut answer)
            .expect("an answer");

        let response: Response = serde_json::from_str(&answer).expect("a response");
        assert!(
            matches!(response, Response::Error { ref message } if message.contains("cannot read the request")),
            "unexpected response: {response:?}"
        );
    }

    /// Proves `read_timeout` is actually honored per-call: a handler that
    /// takes 1.5s outruns a 1s timeout but comfortably finishes inside 3s.
    #[test]
    fn a_read_timeout_shorter_than_the_handler_gives_up() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let socket = dir.path().join("relay.sock");
        let _server = start_server_with(&socket, slow_handler(Duration::from_millis(1500)));

        let err = call(&socket, &Request::Status, Duration::from_secs(1))
            .expect_err("a 1s read timeout must not survive a 1.5s handler");
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
            "unexpected error kind: {:?} ({err})",
            err.kind()
        );
    }

    /// The socket has no authentication of its own, so the file mode is what
    /// keeps every other account on the machine out.
    #[test]
    fn the_socket_is_readable_by_its_owner_alone() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let socket = dir.path().join("relay.sock");
        let _server = start_server(&socket);

        let mode = std::fs::metadata(&socket)
            .expect("the socket exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "unexpected socket mode: {mode:o}");
    }

    /// A client that holds the connection open without asking anything must
    /// not tie up a task forever.
    #[test]
    fn a_client_that_sends_nothing_is_answered_and_dropped() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let socket = dir.path().join("relay.sock");
        let _server = start_server(&socket);

        let stream = SyncUnixStream::connect(&socket).expect("the socket accepts");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("a read timeout");

        let started = Instant::now();
        let mut answer = String::new();
        BufReader::new(stream)
            .read_line(&mut answer)
            .expect("the server answers before it closes");
        let waited = started.elapsed();

        assert!(
            waited < Duration::from_secs(15),
            "the server waited {waited:?} for a request that never came"
        );
        let response: Response = serde_json::from_str(&answer).expect("a response");
        assert!(
            matches!(response, Response::Error { ref message } if message.contains("no request")),
            "unexpected response: {response:?}"
        );
    }

    #[test]
    fn a_line_longer_than_the_limit_comes_back_as_an_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let socket = dir.path().join("relay.sock");
        let _server = start_server(&socket);

        let mut stream = SyncUnixStream::connect(&socket).expect("the socket accepts");
        // Exactly one byte past the limit, which is all the server reads: a
        // longer write would block on a reader that has already given up.
        let oversize = vec![b'a'; MAX_REQUEST_BYTES as usize + 1];
        stream.write_all(&oversize).expect("the write");
        stream.flush().expect("the flush");

        let mut answer = String::new();
        BufReader::new(stream)
            .read_line(&mut answer)
            .expect("an answer");

        let response: Response = serde_json::from_str(&answer).expect("a response");
        assert!(
            matches!(response, Response::Error { ref message } if message.contains("too long")),
            "unexpected response: {response:?}"
        );
    }

    #[test]
    fn a_read_timeout_longer_than_the_handler_still_gets_the_answer() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let socket = dir.path().join("relay.sock");
        let _server = start_server_with(&socket, slow_handler(Duration::from_millis(1500)));

        let response = call(&socket, &Request::Status, Duration::from_secs(3))
            .expect("a 3s read timeout comfortably outlives a 1.5s handler");
        assert_eq!(response, Response::ok(json!({ "cmd": "status" })));
    }
}
