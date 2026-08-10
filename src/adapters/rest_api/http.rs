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
//! * **No keep-alive** — every response carries `Connection: close`.
//! * **No chunked transfer encoding** — a body without `Content-Length` gets a
//!   `411 Length Required`.
//! * `Transfer-Encoding`, `Expect: 100-continue` and query strings are ignored.
//!
//! This is modeled on `src/healthcheck.rs` — the same `Disabled | Enabled` enum,
//! named accept thread, and `self_connect` shutdown trick — with four changes
//! that matter for a surface doing real work: a thread per connection (a slow
//! client must not block every other request), a connection cap, read/write
//! timeouts, and a bounded request size.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::Builder;
use std::time::Duration;

use crate::compile::ast::RawRestApi;

use super::routes::{self, Response};
use super::{Directory, RestApiHandle};

/// Concurrent connections served at once. Unbounded thread spawning is a trivial
/// denial of service; over this limit we answer `503` and close.
const MAX_CONNECTIONS: usize = 16;
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
    },
}

impl RestApiServer {
    pub fn new(
        config: Option<RawRestApi>,
        directory: Arc<Directory>,
        handle: RestApiHandle,
        shutdown_signal: Arc<AtomicBool>,
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
        } = self
        else {
            return Ok(None);
        };

        let listener = TcpListener::bind((host.as_str(), *port))?;
        let addr = listener.local_addr()?;
        let _ = bound.set(addr);

        // The API is unauthenticated (v1), so anyone who can reach the port can
        // control the devices. Binding off-loopback is legitimate behind an
        // authenticating reverse proxy, but it must never happen silently.
        if !is_loopback(host, addr) {
            log::warn!(
                "[rest_api] listening on {addr}, which is NOT loopback. The REST API \
                 is UNAUTHENTICATED: anyone who can reach this port can control your \
                 devices. Bind 127.0.0.1 and put an authenticating reverse proxy in \
                 front of it unless you are certain the network is trusted."
            );
        }

        let directory = Arc::clone(directory);
        let handle = handle.clone();
        let shutdown_signal = Arc::clone(shutdown_signal);
        let live = Arc::clone(live_connections);

        let _ = Builder::new().name("rest_api".to_string()).spawn(move || {
            for stream in listener.incoming() {
                if shutdown_signal.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };

                // Reserve a slot before spawning. Over the cap, answer and close
                // on this thread — cheap, and it keeps the accept loop moving.
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
                let spawned = Builder::new()
                    .name("rest_api_conn".to_string())
                    .spawn(move || {
                        // Decrement even if `serve` panics, so one bad request
                        // cannot permanently consume a connection slot.
                        let _guard = ConnectionGuard(slot);
                        serve(stream, &directory, &handle);
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

/// Serve exactly one request on this connection, then close it.
fn serve(mut stream: TcpStream, directory: &Directory, handle: &RestApiHandle) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    let response = match read_request(&mut stream) {
        Ok(request) => routes::handle(
            directory,
            handle,
            &request.method,
            &request.path,
            &request.body,
        ),
        Err(response) => response,
    };
    let _ = write_response(&mut stream, &response);
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
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
            return Ok(Request { method, path, body });
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
