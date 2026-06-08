mod cgi;
mod config;
mod cookies;
mod http;
mod net;
mod poll;
mod server;

use std::process::ExitCode;

const DEFAULT_CONFIG: &str = "config/default.conf";

fn main() -> ExitCode {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_CONFIG.to_string());

    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read config '{path}': {e}");
            return ExitCode::FAILURE;
        }
    };

    let outcome = config::parse(&src);
    if !outcome.errors.is_empty() {
        eprintln!("config errors ({}):", outcome.errors.len());
        for e in &outcome.errors {
            eprintln!("  - {e}");
        }
    }

    if outcome.config.servers.is_empty() {
        eprintln!("no valid servers in config; nothing to serve");
        return ExitCode::FAILURE;
    }

    let mut server = match server::Server::bind(outcome.config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("startup failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = server.run() {
        eprintln!("event loop terminated: {e}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
