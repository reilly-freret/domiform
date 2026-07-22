
use std::io::{BufReader, prelude::*};
use std::net::{TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::spawn;

use crate::compile::ast::RawHealthcheck;

/// lightweight http healthcheck endpoint
/// 
/// runs only when `system.healthcheck` is set in config
/// 
/// for now, always returns 200 OK, which should be 
///   interpreted as "healthy" (i.e. let the success of the
///   network request determine the health of the system)
pub fn healthcheck_endpoint(
  healthcheck_config: Option<RawHealthcheck>,
  shutdown_signal: Arc<AtomicBool>,
) -> Result<(), std::io::Error> {
  let Some(healthcheck) = healthcheck_config else { return Ok(()) };
  let listener = TcpListener::bind((healthcheck.base.as_str(), healthcheck.port))?;
  spawn(move || {
    for stream in listener.incoming() {
      if shutdown_signal.load(Ordering::SeqCst) { break };
      let Ok(mut stream) = stream else { continue };
      let reader = BufReader::new(&mut stream);
      let Some(Ok(request_line)) = reader.lines().next() else { continue };
      match request_line.as_str() {
        "GET / HTTP/1.1" => {
          let _ = stream.write_all("HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\nHealthy".as_bytes());
        }
        _ => { continue; }
      };
    }
  });
  println!("healthcheck started on {}:{}", healthcheck.base, healthcheck.port);
  Ok(())
}