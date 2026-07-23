use std::io::{prelude::*, BufReader};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::Builder;

use domiform::compile::ast::RawHealthcheck;

/// super-simple healthcheck server. useful for containerized applications.
///
/// when enabled, it responds to all GET / requests with 200 OK "healthy"
///
/// it's up to the user to treat network errors as healthcheck failures
pub enum HealthcheckServer {
    Disabled,
    Enabled {
        host: String,
        port: u16,
        shutdown_signal: Arc<AtomicBool>,
    },
}

impl HealthcheckServer {
    pub fn new(
        config: Option<RawHealthcheck>,
        shutdown_signal: Arc<AtomicBool>,
    ) -> HealthcheckServer {
        let Some(healthcheck) = config else {
            return Self::Disabled;
        };
        Self::Enabled {
            host: healthcheck.host,
            port: healthcheck.port,
            shutdown_signal,
        }
    }

    /// start the server in a simple blocking loop via TcpListener::incoming()
    pub fn start(&self) -> Result<(), std::io::Error> {
        let Self::Enabled {
            host,
            port,
            shutdown_signal,
        } = self
        else {
            return Ok(());
        };
        let listener = TcpListener::bind((host.as_str(), *port))?;
        let shutdown_signal = Arc::clone(shutdown_signal);
        let _ = Builder::new().name("healthcheck".to_string()).spawn(move || {
            for stream in listener.incoming() {
                if shutdown_signal.load(Ordering::SeqCst) {
                    break;
                };
                let Ok(mut stream) = stream else { continue };
                let reader = BufReader::new(&mut stream);
                let Some(Ok(request_line)) = reader.lines().next() else {
                    continue;
                };
                match request_line.as_str() {
                    s if s.starts_with("GET / ") => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nhealthy",
                        );
                    }
                    _ => {
                      let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found",
                      );
                    }
                };
            }
        });
        println!("healthcheck server started on {host}:{port}");
        Ok(())
    }

    /// connect to the server from the same machine
    ///
    /// used to wake up the healthcheck server so that it may
    ///   exit its blocking loop gracefully
    pub fn self_connect(&self) {
        let Self::Enabled {
            port,
            host: _,
            shutdown_signal: _,
        } = self
        else {
            return;
        };
        let _ = TcpStream::connect(("127.0.0.1".to_string(), *port));
    }
}
