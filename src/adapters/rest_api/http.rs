//! The socket layer: a `std::net` HTTP/1.1 server, one thread per connection.
//!
//! # Why not tokio/axum
//!
//! `tokio` is already a dependency, but only with `rt, macros, sync, time, net`,
//! and there is no HTTP server anywhere in the tree. Adding `axum`/`hyper` pulls
//! in hyper, tower, http, http-body and their transitive graph — a real
//! build-time and binary-size cost against a project that deliberately keeps a
//! static musl binary.
//!
//! More importantly the handlers do no async work: a `GET` is a hashmap read
//! under a mutex and a `POST` is a channel send. There is nothing to await.
//!
//! # Scope
//!
//! Only what is needed, with everything else refused explicitly:
//!
//! * HTTP/1.1 request line, headers, and a `Content-Length`-delimited body.
//! * **No keep-alive for request/response** — every such response carries
//!   `Connection: close`. `GET /stream` is the one exception: it holds its
//!   connection open indefinitely and sends no `Content-Length`, because an
//!   event stream has no end.
//! * **No chunked transfer encoding** — a body without `Content-Length` gets a
//!   `411 Length Required`.
//! * `Transfer-Encoding`, `Expect: 100-continue` and query strings are ignored.
//!   Auth is header-only (`Authorization: Bearer …`), deliberately: a token in a
//!   query string leaks into proxy logs and browser history.
//!
//! This is modeled on `src/healthcheck.rs` — the same `Disabled | Enabled` enum,
//! named accept thread, and `self_connect` shutdown trick — with four changes
//! that matter for a surface doing real work: a thread per connection (a slow
//! client must not block every other request), a connection cap, read/write
//! timeouts, and a bounded request size.
//!
//! Streams get their own budget (`MAX_STREAMS`) on top of that cap, so a wall of
//! open dashboard tabs cannot starve ordinary requests.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::Builder;
use std::time::Duration;

use crate::compile::ast::RawRestApi;

use super::routes::{self, Response};
use super::stream::{self, Broadcaster};
use super::{Directory, RestApiHandle};

/// Concurrent request/response connections served at once. Unbounded thread
/// spawning is a trivial denial of service; over this limit we answer `503` and
/// close.
///
/// SSE streams are **not** counted here — see [`MAX_STREAMS`].
const MAX_CONNECTIONS: usize = 16;
/// Concurrent SSE streams, budgeted separately from [`MAX_CONNECTIONS`].
///
/// Streams are long-lived, so charging them against the request/response cap
/// would let a handful of open dashboard tabs exhaust it and `503` the entire
/// API — including the plain `GET`s. A separate budget keeps one from starving
/// the other.
const MAX_STREAMS: usize = 8;
/// How long a stream writer waits for a frame before emitting a keepalive
/// comment. Keeps intermediaries from reaping an idle connection, and surfaces a
/// dead peer as a write error.
const STREAM_KEEPALIVE: Duration = Duration::from_secs(30);
/// Applied to every accepted stream, so a client that opens a socket and never
/// sends cannot pin a thread indefinitely.
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest request body accepted, in bytes.
const MAX_BODY: usize = 64 * 1024;
/// Caps on the header section, to bound work before `Content-Length` is known.
const MAX_HEADER_LINES: usize = 64;
const MAX_HEADER_BYTES: usize = 8 * 1024;

pub enum RestApiServer {
    Disabled,
    Enabled {
        host: String,
        port: u16,
        directory: Arc<Directory>,
        handle: RestApiHandle,
        shutdown_signal: Arc<AtomicBool>,
        /// Set by [`start`](RestApiServer::start), so `self_connect` works when
        /// the config asked for port 0 (tests).
        bound: Arc<OnceLock<SocketAddr>>,
        live_connections: Arc<AtomicUsize>,
        /// The SSE fan-out. Streams register here; the engine-side observer
        /// pushes into it.
        broadcaster: Broadcaster,
        /// The bearer token, when configured. `None` leaves every route open
        /// (v1 behavior); `Some` requires it on *every* route.
        token: Option<String>,
    },
}

impl RestApiServer {
    pub fn new(
        config: Option<RawRestApi>,
        directory: Arc<Directory>,
        handle: RestApiHandle,
        shutdown_signal: Arc<AtomicBool>,
        broadcaster: Broadcaster,
    ) -> Self {
        let Some(config) = config else {
            return Self::Disabled;
        };
        Self::Enabled {
            host: config.host,
            port: config.port,
            directory,
            handle,
            shutdown_signal,
            bound: Arc::new(OnceLock::new()),
            live_connections: Arc::new(AtomicUsize::new(0)),
            broadcaster,
            token: config.token,
        }
    }

    /// Bind and spawn the accept thread. Returns the bound address, or `None`
    /// when disabled.
    ///
    /// Binding happens on the caller's thread so a misconfigured port is an
    /// immediate, reportable error rather than a failure buried in a background
    /// thread.
    pub fn start(&self) -> std::io::Result<Option<SocketAddr>> {
        let Self::Enabled {
            host,
            port,
            directory,
            handle,
            shutdown_signal,
            bound,
            live_connections,
            broadcaster,
            token,
        } = self
        else {
            return Ok(None);
        };

        let listener = TcpListener::bind((host.as_str(), *port))?;
        let addr = listener.local_addr()?;
        let _ = bound.set(addr);

        // An off-loopback bind is legitimate behind a token or an authenticating
        // reverse proxy, but an *unauthenticated* one must never happen silently:
        // anyone who can reach the port could control the devices.
        if !is_loopback(host, addr) && token.is_none() {
            log::warn!(
                "[rest_api] listening on {addr}, which is NOT loopback. The REST API \
                 is UNAUTHENTICATED: anyone who can reach this port can control your \
                 devices. Set `system.rest_api.token`, or bind 127.0.0.1 and put an \
                 authenticating reverse proxy in front of it, unless you are certain \
                 the network is trusted."
            );
        }

        let directory = Arc::clone(directory);
        let handle = handle.clone();
        let shutdown_signal = Arc::clone(shutdown_signal);
        let live = Arc::clone(live_connections);
        let broadcaster = broadcaster.clone();
        let token = token.clone();

        let _ = Builder::new().name("rest_api".to_string()).spawn(move || {
            for stream in listener.incoming() {
                if shutdown_signal.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };

                // Reserve a slot before spawning. Over the cap, answer and close
                // on this thread — cheap, and it keeps the accept loop moving.
                //
                // A stream request is *also* accepted under this budget, then
                // handed its own on upgrade: the request must be read before we
                // can know it is a stream at all.
                if live.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    let mut stream = stream;
                    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                    let _ = write_response(
                        &mut stream,
                        &Response::error(
                            503,
                            "too_many_connections",
                            "too many concurrent connections; retry shortly",
                        ),
                    );
                    continue;
                }
                live.fetch_add(1, Ordering::SeqCst);

                let directory = Arc::clone(&directory);
                let handle = handle.clone();
                let slot = Arc::clone(&live);
                let broadcaster = broadcaster.clone();
                let token = token.clone();
                let spawned = Builder::new()
                    .name("rest_api_conn".to_string())
                    .spawn(move || {
                        // Decrement even if `serve` panics, so one bad request
                        // cannot permanently consume a connection slot.
                        let _guard = ConnectionGuard(slot);
                        serve(stream, &directory, &handle, &broadcaster, token.as_deref());
                    });
                // The slot was reserved above, and the guard that would release
                // it never came into existence if the spawn failed.
                if spawned.is_err() {
                    live.fetch_sub(1, Ordering::SeqCst);
                }
            }
        });

        println!("rest_api server started on {addr}");
        Ok(Some(addr))
    }

    /// Connect to the server from the same machine, so its blocking `accept`
    /// returns and the loop can observe the shutdown flag and exit.
    pub fn self_connect(&self) {
        let Self::Enabled { bound, port, .. } = self else {
            return;
        };
        // Prefer the actually-bound address: with `port: 0` the configured port
        // is meaningless, and a bound address is also right when the host is a
        // specific interface.
        match bound.get() {
            Some(addr) => {
                let _ = TcpStream::connect(addr);
            }
            None => {
                let _ = TcpStream::connect(("127.0.0.1", *port));
            }
        }
    }
}

/// Releases a connection slot on drop, including on panic.
struct ConnectionGuard(Arc<AtomicUsize>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whether the server is bound to a loopback address — either literally, or via
/// a hostname that resolves entirely to loopback.
fn is_loopback(host: &str, addr: SocketAddr) -> bool {
    if addr.ip().is_loopback() {
        return true;
    }
    // A wildcard bind (0.0.0.0 / ::) reports an unspecified address, which is
    // reachable from the network — decidedly not loopback.
    if addr.ip().is_unspecified() {
        return false;
    }
    (host, 0)
        .to_socket_addrs()
        .map(|mut addrs| addrs.all(|a| a.ip().is_loopback()))
        .unwrap_or(false)
}

/// Serve one request on this connection.
///
/// Normally that means one response and close. `GET /stream` is the exception:
/// it upgrades to a long-lived SSE connection and this function does not return
/// until the client disconnects or the host shuts down.
fn serve(
    mut stream: TcpStream,
    directory: &Directory,
    handle: &RestApiHandle,
    broadcaster: &Broadcaster,
    token: Option<&str>,
) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(response) => {
            let _ = write_response(&mut stream, &response);
            return;
        }
    };

    // Auth first, before routing: when a token is configured it is required on
    // *every* route, so no endpoint can be reached by an unauthenticated caller.
    if let Some(expected) = token {
        if let Err(response) = check_auth(expected, request.authorization.as_deref()) {
            let _ = write_response(&mut stream, &response);
            return;
        }
    }

    if request.method == "GET" && request.path.trim_end_matches('/') == "/stream" {
        serve_stream(stream, directory, handle, broadcaster);
        return;
    }

    let response = routes::handle(
        directory,
        handle,
        &request.method,
        &request.path,
        &request.body,
    );
    let _ = write_response(&mut stream, &response);
}

/// Validate the `Authorization` header against the configured token.
///
/// `401` when the header is absent or not a well-formed bearer credential;
/// `403` when it is well-formed but wrong — the distinction a client needs to
/// tell "I forgot to authenticate" from "my token is not accepted".
fn check_auth(expected: &str, provided: Option<&str>) -> Result<(), Response> {
    let Some(header) = provided else {
        return Err(Response::error(
            401,
            "unauthorized",
            "this API requires an 'Authorization: Bearer <token>' header",
        ));
    };
    // The scheme is case-insensitive per RFC 7235; the credential is not.
    let Some(candidate) = header
        .split_once(char::is_whitespace)
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, rest)| rest.trim())
    else {
        return Err(Response::error(
            401,
            "unauthorized",
            "malformed Authorization header; expected 'Bearer <token>'",
        ));
    };
    if constant_time_eq(candidate.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(Response::error(403, "forbidden", "invalid token"))
    }
}

/// Compare two secrets without leaking their contents through timing.
///
/// A plain `==` short-circuits on the first differing byte, letting an attacker
/// recover a token one byte at a time. This folds over the whole of both slices
/// unconditionally. The *length* is not secret (and cannot be hidden by this
/// construction anyway), so an early length check is fine.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
    authorization: Option<String>,
}

/// Parse one HTTP/1.1 request. Returns the routed error response directly on any
/// malformed or oversized input, so `serve` has a single write path.
fn read_request(stream: &mut TcpStream) -> Result<Request, Response> {
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return Err(Response::error(400, "malformed_body", "empty request"));
    }

    // `METHOD SP PATH SP VERSION`
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(Response::error(
            400,
            "malformed_body",
            "malformed request line",
        ));
    };
    // Query strings are ignored in v1; strip so `/devices?x=1` routes as
    // `/devices` rather than falling through to `unknown_route`.
    let path = target
        .split(['?', '#'])
        .next()
        .unwrap_or(target)
        .to_string();
    let method = method.to_string();

    // Headers, bounded in both count and bytes.
    let mut content_length: Option<usize> = None;
    let mut authorization: Option<String> = None;
    let mut header_bytes = 0usize;
    for _ in 0..MAX_HEADER_LINES {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(n) => header_bytes += n,
            Err(_) => return Err(Response::error(400, "malformed_body", "malformed headers")),
        }
        if header_bytes > MAX_HEADER_BYTES {
            return Err(Response::error(
                431,
                "payload_too_large",
                "header section too large",
            ));
        }
        let line = line.trim_end();
        if line.is_empty() {
            // End of the header section.
            let length = content_length.unwrap_or(0);
            if length > MAX_BODY {
                return Err(Response::error(
                    413,
                    "payload_too_large",
                    format!("body of {length} bytes exceeds the {MAX_BODY} byte limit"),
                ));
            }
            // Read exactly `Content-Length` bytes; never read to EOF.
            let mut body = vec![0u8; length];
            if length > 0 && reader.read_exact(&mut body).is_err() {
                return Err(Response::error(
                    400,
                    "malformed_body",
                    "request body was shorter than its Content-Length",
                ));
            }
            return Ok(Request {
                method,
                path,
                body,
                authorization,
            });
        }

        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                match value.trim().parse::<usize>() {
                    Ok(n) => content_length = Some(n),
                    Err(_) => {
                        return Err(Response::error(
                            400,
                            "malformed_body",
                            "invalid Content-Length",
                        ))
                    }
                }
            } else if name.trim().eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_string());
            } else if name.trim().eq_ignore_ascii_case("transfer-encoding") {
                // Chunked bodies are out of scope; say so rather than
                // misinterpreting the framing.
                return Err(Response::error(
                    411,
                    "malformed_body",
                    "chunked transfer encoding is not supported; send Content-Length",
                ));
            }
        }
    }

    Err(Response::error(
        431,
        "payload_too_large",
        "too many header lines",
    ))
}

/// Serve `GET /stream` as server-sent events until the client goes away.
///
/// This is the one path that does not follow the "one request, one response,
/// close" shape: it holds the connection open, so it needs its own header block
/// (no `Content-Length`, since the body has no end) and its own budget.
fn serve_stream(
    mut stream: TcpStream,
    directory: &Directory,
    handle: &RestApiHandle,
    broadcaster: &Broadcaster,
) {
    if broadcaster.subscriber_count() >= MAX_STREAMS {
        let _ = write_response(
            &mut stream,
            &Response::error(
                503,
                "too_many_streams",
                "too many concurrent streams; close one and retry",
            ),
        );
        return;
    }

    // Subscribe *before* building the snapshot. Deltas that land while we
    // serialize queue behind it and are delivered after — a superseded value is
    // harmless, a missing one is not. Subscribing after would reopen the
    // lost-update window the snapshot exists to close.
    //
    // Dropping this deregisters the subscriber and frees the slot, including on
    // an early return or a panic, so a dead client is reaped without the engine
    // ever learning it existed.
    let subscription = broadcaster.subscribe();

    let head = "HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\n\
                Connection: keep-alive\r\n\r\n";
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }

    // The snapshot is built here, on this connection's thread — never on the
    // engine's.
    let snapshot = stream::snapshot_frame(routes::devices_value(directory, handle));
    if stream.write_all(snapshot.encode().as_bytes()).is_err() {
        return;
    }
    if stream.flush().is_err() {
        return;
    }

    // A stream has no further request to read, so the read timeout is irrelevant;
    // the write timeout stays, so a wedged peer is eventually reaped.
    let _ = stream.set_read_timeout(None);

    loop {
        match subscription.next_batch(STREAM_KEEPALIVE) {
            stream::Batch::Frames { frames, lagged } => {
                // Tell the client its view has a hole *before* the frames that
                // followed the gap, so it can re-sync at the right point.
                if lagged
                    && stream
                        .write_all(stream::lagged_frame().encode().as_bytes())
                        .is_err()
                {
                    return;
                }
                for frame in frames {
                    if stream.write_all(frame.encode().as_bytes()).is_err() {
                        return;
                    }
                }
                if stream.flush().is_err() {
                    return;
                }
            }
            // An SSE comment: keeps intermediaries from reaping an idle
            // connection, and turns a dead peer into a write error we can see.
            stream::Batch::Idle => {
                if stream.write_all(b": keepalive\n\n").is_err() || stream.flush().is_err() {
                    return;
                }
            }
            stream::Batch::Done => return,
        }
    }
}

fn write_response(stream: &mut TcpStream, response: &Response) -> std::io::Result<()> {
    // No CORS headers, deliberately: a browser page on another origin must not be
    // able to drive someone's house, and omitting the headers is what prevents it.
    let head = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        response.status,
        reason_phrase(response.status),
        response.body.len(),
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "OK",
    }
}
