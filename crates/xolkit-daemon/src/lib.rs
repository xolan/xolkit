//! The xolkit daemon, the `xolkit mcp` shim agents start, and the app's client for the daemon.

mod client;
mod daemon;
mod gateway;
mod ipc;
mod protocol;
mod session;
mod shim;

use std::path::Path;

use anyhow::Result;
use xolkit_core::{Agent, Env};

pub use client::Client;
pub use protocol::{
    Event, Hello, Parent, Request, Response, ServerState, ServerStatus, SessionStatus, Status,
};

/// Connects to the daemon of `env`, if it runs, without starting it.
pub async fn connect(env: &Env) -> std::io::Result<interprocess::local_socket::tokio::Stream> {
    ipc::connect_async(env).await
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

/// `xolkit daemon`: runs until it is told to stop or has been idle long enough.
pub fn run_daemon(env: Env) -> Result<()> {
    runtime()?.block_on(daemon::run(env))
}

/// `xolkit mcp`: serves one agent session through the daemon, which `exe` starts when needed.
pub fn run_shim(env: Env, exe: &Path, agent: Option<Agent>) -> Result<()> {
    let runtime = runtime()?;
    let result = runtime.block_on(shim::run(env, exe, agent));
    // Without waiting for the thread reading stdin: when the shim leaves because its agent is
    // gone, something else may still hold stdin open, and that read never returns.
    runtime.shutdown_background();
    result
}
