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
        /// The process that started the shim: the agent, or the app hosting it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<Parent>,
    },
    Control,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Parent {
    pub pid: u32,
    /// Its program's name, where the OS says (Linux).
    pub name: Option<String>,
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
    /// connect again at once.
    SetSecret {
        key: String,
        value: Option<String>,
        #[serde(default = "yes")]
        reconnect: bool,
    },
    /// Saves a server's secrets and runs `actions` (its settings) as one change: the server is
    /// disconnected first, and connects once, after both are saved. If anything fails, the
    /// secrets are put back as they were, so a new secret never pairs with an old address.
    SaveServer {
        name: String,
        /// Keyring keys and values; `None` deletes.
        secrets: Vec<(String, Option<String>)>,
        actions: Vec<Action>,
    },
    /// Reconnects to an MCP server, or connects to one that failed or was stopped.
    RestartServer {
        name: String,
    },
    StopServer {
        name: String,
    },
    /// Ends an agent session; its shim connects again at once, as a new session. An idle one is
    /// forgotten until its agent's next message.
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
    /// Minutes a session may stay quiet before its shim lets go; `0` for never. `None` from a
    /// daemon older than 3.1.
    #[serde(default)]
    pub session_idle_minutes: Option<u64>,
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
    #[serde(default)]
    pub parent: Option<Parent>,
    pub connected: u64,
    pub last_call: Option<u64>,
    pub calls: u64,
    pub errors: u64,
    /// Whether the shim has let go of the daemon after a quiet spell. It is still running, and
    /// connects again with the agent's next message.
    #[serde(default)]
    pub idle: bool,
    /// When the session last connected or went idle.
    #[serde(default)]
    pub since: u64,
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
            parent: None,
        };
        let line = serde_json::to_string(&hello).unwrap();
        assert_eq!(
            line,
            r#"{"kind":"session","agent":"codex","pid":7,"cwd":null}"#
        );
        assert_eq!(serde_json::from_str::<Hello>(&line).unwrap(), hello);
        let hello = Hello::Session {
            agent: None,
            pid: 7,
            cwd: None,
            parent: Some(Parent {
                pid: 6,
                name: Some("codex".into()),
            }),
        };
        let line = serde_json::to_string(&hello).unwrap();
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
