//! `xolkit mcp`: what an agent starts for each session. It passes MCP messages between the
//! agent's stdio and the daemon, starting the daemon when it isn't running. When the daemon
//! goes away (it restarts after an update, say), the shim answers what was in flight with an
//! error, connects again and replays the session's opening, so the agent keeps working.
//!
//! A session that stays quiet lets go of the daemon, so the daemon can exit and stop its MCP
//! servers; the agent's next message opens it again the same way. The shim exits when the agent
//! closes its end, or when the agent's process is gone.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, Stdin};
use tokio::time::Instant;
use xolkit_core::{Agent, Env, Settings};

use crate::ipc;
use crate::protocol::{Hello, Parent};

/// How many times in a row the shim tries to reach a daemon before it gives up.
const RECONNECTS: u32 = 5;
/// How often the shim checks that the agent that started it still runs.
const PARENT_CHECK: Duration = Duration::from_secs(5);

/// What ended a connection to the daemon.
enum Ended {
    /// The daemon went away.
    Daemon,
    /// Nothing happened for the idle time: the shim let go.
    Idle,
}

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

    /// Whether the session can let go of the daemon: it has opened and waits on nothing.
    fn can_idle(&self) -> bool {
        self.initialized.is_some() && self.pending.is_empty()
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
    let parent = parent();
    let hello = serde_json::to_string(&Hello::Session {
        agent,
        pid: std::process::id(),
        cwd: std::env::current_dir().ok(),
        parent: parent.clone(),
    })? + "\n";
    let parent = parent.map(|p| p.pid);
    // A line from the agent that came while the shim was idle, to send once it is connected.
    let mut waiting: Option<String> = None;
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
        if let Some(line) = &waiting {
            opening.push_str(line);
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
        if waiting.take().is_some() {
            // The agent missed what changed while the shim was away: have it list again.
            for method in [
                "notifications/tools/list_changed",
                "notifications/prompts/list_changed",
            ] {
                let line = json!({"jsonrpc": "2.0", "method": method}).to_string();
                stdout.write_all(format!("{line}\n").as_bytes()).await?;
            }
            stdout.flush().await?;
        }
        // Read again on each connection, so a change in the app applies from the next one.
        let idle_after = Settings::load(&env.settings_file())
            .map(|s| s.session_idle_minutes)
            .unwrap_or(0);
        let idle_after = (idle_after > 0).then(|| Duration::from_secs(idle_after * 60));
        let connected = std::time::Instant::now();
        let mut daemon_lines = BufReader::new(recv).lines();
        let mut last_heard = Instant::now();
        let mut parent_check = tokio::time::interval(PARENT_CHECK);
        let ended = loop {
            let quiet = async {
                match idle_after {
                    Some(after) => tokio::time::sleep_until(last_heard + after).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                line = agent_lines.next_line() => match line {
                    Ok(Some(line)) => {
                        last_heard = Instant::now();
                        session.heard_from_agent(&line);
                        if send.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                            break Ended::Daemon;
                        }
                    }
                    // The agent closed the session.
                    _ => return Ok(()),
                },
                line = daemon_lines.next_line() => match line {
                    Ok(Some(line)) => {
                        last_heard = Instant::now();
                        if session.heard_from_daemon(&line) {
                            stdout.write_all(format!("{line}\n").as_bytes()).await?;
                            stdout.flush().await?;
                        }
                    }
                    _ => break Ended::Daemon,
                },
                () = quiet => {
                    if session.can_idle() {
                        break Ended::Idle;
                    }
                    last_heard = Instant::now();
                }
                _ = parent_check.tick() => {
                    if parent_gone(parent) {
                        return Ok(());
                    }
                }
            }
        };
        if let Ended::Idle = ended {
            drop((send, daemon_lines));
            match wait_idle(&mut agent_lines, parent).await {
                Some(line) => {
                    session.heard_from_agent(&line);
                    waiting = Some(line);
                    failures = 0;
                    continue;
                }
                None => return Ok(()),
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

/// Waits, connected to nothing, for the agent's next line; `None` once the agent is gone.
async fn wait_idle(
    agent_lines: &mut Lines<BufReader<Stdin>>,
    parent: Option<u32>,
) -> Option<String> {
    let mut parent_check = tokio::time::interval(PARENT_CHECK);
    loop {
        tokio::select! {
            line = agent_lines.next_line() => return line.ok().flatten(),
            _ = parent_check.tick() => {
                if parent_gone(parent) {
                    return None;
                }
            }
        }
    }
}

/// The process that started the shim.
fn parent() -> Option<Parent> {
    #[cfg(unix)]
    {
        let pid = std::os::unix::process::parent_id();
        #[cfg(target_os = "linux")]
        let name = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty());
        #[cfg(not(target_os = "linux"))]
        let name = None;
        Some(Parent { pid, name })
    }
    #[cfg(not(unix))]
    None
}

/// Whether the agent that started the shim has exited: the shim then belongs to another
/// process (init, or a subreaper), though something may still hold its stdin open.
fn parent_gone(parent: Option<u32>) -> bool {
    #[cfg(unix)]
    {
        parent.is_some_and(|pid| std::os::unix::process::parent_id() != pid)
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        false
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

    #[test]
    fn idles_only_once_open_and_answered() {
        let mut session = Session::default();
        assert!(!session.can_idle());
        session.heard_from_agent(r#"{"jsonrpc":"2.0","id":0,"method":"initialize"}"#);
        session.heard_from_daemon(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#);
        session.heard_from_agent(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert!(session.can_idle());
        session.heard_from_agent(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call"}"#);
        assert!(!session.can_idle());
        session.heard_from_daemon(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        assert!(session.can_idle());
    }
}
