//! What travels over the daemon's socket. Every connection starts with one line of [`Hello`].
//! A session then speaks MCP; a control connection (the app) sends one [`Request`] per line
//! and gets one [`Response`] line back.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use xolkit_core::{Action, Agent};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Hello {
    /// A shim, for an agent session.
    Session {
        /// The agent it was registered for, when it says.
        agent: Option<Agent>,
        /// The shim's process.
        pid: u32,
        /// The folder the agent started it in: the session's project.
        cwd: Option<PathBuf>,
    },
    Control,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "camelCase")]
pub enum Request {
    Status,
    /// Runs the actions in order, then reloads the library.
    Execute {
        actions: Vec<Action>,
    },
    /// Saves a secret in the OS keyring, or deletes it. With `reconnect`, servers using it
    /// connect again at once; without, the caller sends `RefreshServer` once the server's
    /// settings are saved too, so a new secret never goes to an old address.
    SetSecret {
        key: String,
        value: Option<String>,
        #[serde(default = "yes")]
        reconnect: bool,
    },
    /// Connects to a server again with its current settings and secrets, unless it was
    /// stopped by hand.
    RefreshServer {
        name: String,
    },
    /// Reconnects to an MCP server, or connects to one that failed or was stopped.
    RestartServer {
        name: String,
    },
    StopServer {
        name: String,
    },
    /// Ends an agent session; the agent may start a new one.
    Disconnect {
        session: u64,
    },
    /// Stops the daemon. Shims start a new one when they next need it.
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "camelCase")]
pub enum Response {
    Ok,
    Status(Box<Status>),
    Error { message: String },
}

/// Everything the Connections page shows.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub pid: u32,
    pub version: String,
    /// Seconds since the Unix epoch.
    pub started: u64,
    pub idle_minutes: u64,
    pub sessions: Vec<SessionStatus>,
    pub servers: Vec<ServerStatus>,
    /// Newest last.
    pub events: Vec<Event>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatus {
    pub id: u64,
    pub agent: Agent,
    /// The client's own name and version, once it has said.
    pub client: Option<String>,
    /// The shim's process.
    pub pid: u32,
    pub cwd: Option<PathBuf>,
    pub connected: u64,
    pub last_call: Option<u64>,
    pub calls: u64,
    pub errors: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub name: String,
    /// The plugin it comes with, by id.
    pub plugin: Option<String>,
    pub remote: bool,
    pub state: ServerState,
    pub pid: Option<u32>,
    pub tools: usize,
    pub calls: u64,
    /// How long the last call took.
    pub latency_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum ServerState {
    Starting,
    Running,
    /// Waiting for a secret to be saved, named.
    NeedsSecret {
        key: String,
    },
    Failed {
        message: String,
    },
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    /// Seconds since the Unix epoch.
    pub time: u64,
    /// What it is about: `daemon`, a session (`session 3`), or a server.
    pub source: String,
    pub message: String,
    pub error: bool,
}

fn yes() -> bool {
    true
}

pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_tagged_lines() {
        let hello = Hello::Session {
            agent: Some(Agent::Codex),
            pid: 7,
            cwd: None,
        };
        let line = serde_json::to_string(&hello).unwrap();
        assert_eq!(
            line,
            r#"{"kind":"session","agent":"codex","pid":7,"cwd":null}"#
        );
        assert_eq!(serde_json::from_str::<Hello>(&line).unwrap(), hello);
        let request = serde_json::to_string(&Request::Disconnect { session: 3 }).unwrap();
        assert_eq!(request, r#"{"request":"disconnect","session":3}"#);
        let error = Response::Error {
            message: "no".into(),
        };
        let line = serde_json::to_string(&error).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&line).unwrap(), error);
    }
}
