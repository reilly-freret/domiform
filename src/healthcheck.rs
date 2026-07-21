/// lightweight http healthcheck endpoint
/// runs only when `system.healthcheck` is set in config
/// for now, always returns 200 OK, which can be interpreted as "healthy"

use std::net::{TcpListener, TcpStream};
use std::io::{BufReader};

use crate::compile::SystemConfig;
use crate::compile::ast::RawHealthcheck;

// todo: this should probably return a Result<thread, error>
pub fn healthcheck_endpoint(healthcheck_config: Option<RawHealthcheck>) {
  let Some(healthcheck) = healthcheck_config else { return };
  let listener = TcpListener::bind((healthcheck.base.as_str(), healthcheck.port))
    .expect(&format!("unable to bind to healthcheck endpoint [{}:{}]", healthcheck.base, healthcheck.port));
  // spawn thread loop
  println!("healthcheck started on {}:{}", healthcheck.base, healthcheck.port);
}