//! `xolkit mcp`: what an agent starts for each session. It passes MCP messages between the
//! agent's stdio and the daemon, starting the daemon when it isn't running. When the daemon
//! goes away (it restarts after an update, say), the shim answers what was in flight with an
//! error, connects again and replays the session's opening, so the agent keeps working.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use xolkit_core::{Agent, Env};

use crate::ipc;
use crate::protocol::Hello;

/// How many times in a row the shim tries to reach a daemon before it gives up.
const RECONNECTS: u32 = 5;

/// What the shim remembers of the session, to replay it to a new daemon.
#[derive(Default)]
struct Session {
    /// The agent's `initialize` request, and its `notifications/initialized`.
    initialize: Option<String>,
    initialized: Option<String>,
    /// Requests sent and not answered yet, by id, with the id as sent.
    pending: HashMap<String, Value>,
    /// The id of a replayed `initialize`, whose answer the agent must not see.
    swallow: Option<String>,
}

impl Session {
    /// Notes a line from the agent.
    fn heard_from_agent(&mut self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return;
        };
        match message["method"].as_str() {
            Some("initialize") => self.initialize = Some(line.to_owned()),
            Some("notifications/initialized") => self.initialized = Some(line.to_owned()),
            _ => {}
        }
        if message.get("method").is_some()
            && let Some(id) = message.get("id")
        {
            self.pending.insert(id.to_string(), id.clone());
        }
    }

    /// Notes a line from the daemon; `false` when the agent shouldn't see it.
    fn heard_from_daemon(&mut self, line: &str) -> bool {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return true;
        };
        if message.get("method").is_some() {
            return true;
        }
        let Some(id) = message.get("id").map(Value::to_string) else {
            return true;
        };
        if self.swallow.as_ref() == Some(&id) {
            self.swallow = None;
            return false;
        }
        self.pending.remove(&id);
        true
    }

    /// Error answers for every request the daemon took with it.
    fn abandon(&mut self) -> Vec<String> {
        self.pending
            .drain()
            .map(|(_, id)| {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32603, "message": "xolkit restarted; try again"}
                })
                .to_string()
            })
            .collect()
    }

    /// The lines that open the session again on a new daemon.
    fn replay(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(initialize) = &self.initialize {
            self.swallow = serde_json::from_str::<Value>(initialize)
                .ok()
                .and_then(|m| m.get("id").map(Value::to_string));
            lines.push(initialize.clone());
        }
        lines.extend(self.initialized.clone());
        lines
    }
}

/// Runs the shim until the agent closes its end.
pub async fn run(env: Env, exe: &Path, agent: Option<Agent>) -> Result<()> {
    let mut agent_lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut session = Session::default();
    let hello = serde_json::to_string(&Hello::Session {
        agent,
        pid: std::process::id(),
        cwd: std::env::current_dir().ok(),
    })? + "\n";
    let mut failures = 0;
    loop {
        let stream = match ipc::connect_or_start(&env, exe).await {
            Ok(stream) => stream,
            Err(e) if failures >= RECONNECTS => return Err(e).context("could not reach xolkit"),
            Err(_) => {
                failures += 1;
                tokio::time::sleep(Duration::from_millis(500 * u64::from(failures))).await;
                continue;
            }
        };
        let (recv, mut send) = tokio::io::split(stream);
        let mut opening = hello.clone();
        for line in session.replay() {
            opening.push_str(&line);
            opening.push('\n');
        }
        if send.write_all(opening.as_bytes()).await.is_err() {
            failures += 1;
            if failures > RECONNECTS {
                anyhow::bail!("xolkit keeps closing the session");
            }
            tokio::time::sleep(Duration::from_millis(500 * u64::from(failures))).await;
            continue;
        }
        let connected = std::time::Instant::now();
        let mut daemon_lines = BufReader::new(recv).lines();
        loop {
            tokio::select! {
                line = agent_lines.next_line() => match line {
                    Ok(Some(line)) => {
                        session.heard_from_agent(&line);
                        if send.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    // The agent closed the session.
                    _ => return Ok(()),
                },
                line = daemon_lines.next_line() => match line {
                    Ok(Some(line)) => {
                        if session.heard_from_daemon(&line) {
                            stdout.write_all(format!("{line}\n").as_bytes()).await?;
                            stdout.flush().await?;
                        }
                    }
                    _ => break,
                },
            }
        }
        // A connection that keeps ending at once counts as a failure, so the shim backs off and
        // gives up instead of spinning.
        if connected.elapsed() > Duration::from_secs(10) {
            failures = 0;
        } else {
            failures += 1;
            if failures > RECONNECTS {
                anyhow::bail!("xolkit keeps closing the session");
            }
            tokio::time::sleep(Duration::from_millis(500 * u64::from(failures))).await;
        }
        for line in session.abandon() {
            stdout.write_all(format!("{line}\n").as_bytes()).await?;
        }
        stdout.flush().await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replays_the_opening_and_answers_what_was_lost() {
        let mut session = Session::default();
        let init = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#;
        session.heard_from_agent(init);
        assert!(session.heard_from_daemon(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#));
        session.heard_from_agent(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        session.heard_from_agent(r#"{"jsonrpc":"2.0","id":"a","method":"tools/call"}"#);
        session.heard_from_agent(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        assert!(session.heard_from_daemon(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#));

        let lost = session.abandon();
        assert_eq!(lost.len(), 1);
        assert!(lost[0].contains(r#""id":"a""#), "{}", lost[0]);
        assert_eq!(session.replay().len(), 2);
        // The new daemon's answer to the replayed opening isn't passed on; later ones are.
        assert!(!session.heard_from_daemon(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#));
        assert!(session.heard_from_daemon(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#));
        assert!(
            session.heard_from_daemon(
                r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#
            )
        );
    }
}
