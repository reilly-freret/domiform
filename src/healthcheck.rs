use std::io::{prelude::*, BufReader};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::spawn;

use crate::compile::ast::RawHealthcheck;

pub struct HealthcheckServer {
    base: Option<String>,
    port: Option<u16>,
    shutdown_signal: Arc<AtomicBool>,
    enabled: bool,
}

/// super-simple healthcheck server. useful for containerized applications.
///
/// when enabled, it responds to all requests with 200 OK "healthy"
///
/// it's up to the user to treat network errors as healthcheck failures
impl HealthcheckServer {
    pub fn new(
        config: Option<RawHealthcheck>,
        shutdown_signal: Arc<AtomicBool>,
    ) -> HealthcheckServer {
        let Some(healthcheck) = config else {
            return Self {
                base: None,
                port: None,
                shutdown_signal,
                enabled: false,
            };
        };
        Self {
            base: Some(healthcheck.base),
            port: Some(healthcheck.port),
            shutdown_signal,
            enabled: true,
        }
    }
    pub fn start(&self) -> Result<(), std::io::Error> {
        if !self.enabled {
            return Ok(());
        };
        let (Some(base), Some(port)) = (self.base.as_deref(), self.port) else {
            return Ok(());
        };
        let listener = TcpListener::bind((base, port))?;
        let shutdown_signal = Arc::clone(&self.shutdown_signal);
        spawn(move || {
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
                    "GET / HTTP/1.1" => {
                        let _ = stream.write_all(
                            "HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nhealthy".as_bytes(),
                        );
                    }
                    _ => {
                        continue;
                    }
                };
            }
        });
        println!("healthcheck server started on {base}:{port}");
        Ok(())
    }
    pub fn self_connect(&self) {
        if !self.enabled {
            return;
        };
        let (Some(base), Some(port)) = (self.base.as_deref(), self.port) else {
            return;
        };
        let _ = TcpStream::connect((base, port));
    }
}
