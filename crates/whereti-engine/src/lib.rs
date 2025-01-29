//! The whereti engine: index + live updates + search service (PLAN.md §3).
//!
//! Used by the Windows service and by `wt serve` (foreground, for testing).

#![cfg(windows)]

pub mod engine;
pub mod log;
pub mod sync;
pub mod system;

use std::sync::Arc;
use std::time::{Duration, Instant};

pub use engine::{Config, Engine};
pub use log::Logger;

/// Runs the engine and the pipe server until `seconds` pass (or forever).
pub fn run_foreground(cfg: Config, seconds: Option<u64>) -> Result<(), String> {
    let engine = Engine::start(cfg, Logger::console());
    let handler: whereti_ipc::Handler = {
        let engine = engine.clone();
        Arc::new(move |req| engine.handle(req))
    };
    let server = whereti_ipc::Server::start(handler).map_err(|e| {
        engine.stop();
        format!("cannot open {}: {e} (is the whereti service already running?)", whereti_ipc::PIPE_NAME)
    })?;
    engine.log(format!("listening on {}", whereti_ipc::PIPE_NAME));
    let started = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(200));
        if seconds.is_some_and(|s| started.elapsed() >= Duration::from_secs(s)) {
            break;
        }
    }
    server.stop();
    engine.stop();
    Ok(())
}
