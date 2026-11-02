//! Connections to the MCP servers xolkit passes on: local ones it starts, remote ones over
//! streamable HTTP. One connection per server, shared by every session.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig, Implementation, Tool};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use xolkit_core::{Env, Server, ServerConfig, get_secret, secret_key};

use crate::protocol::{ServerState, ServerStatus};

/// How long a server gets to start and list its tools.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long one tool call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Why a server in `Gateway::held` stays disconnected.
const HELD: &str = "a save didn't finish; save the server again";

/// Something the gateway wants the daemon to know.
pub(crate) enum Notice {
    /// A server's tools changed, or it connected or went away.
    ToolsChanged,
    /// A line for the event log, about server `source`.
    Log {
        source: String,
        message: String,
        error: bool,
    },
}

pub(crate) type Notify = Arc<dyn Fn(Notice) + Send + Sync>;

struct Connection {
    server: Server,
    state: ServerState,
    service: Option<Arc<RunningService<RoleClient, Handler>>>,
    tools: Vec<Tool>,
    pid: Option<u32>,
    calls: u64,
    latency_ms: Option<u64>,
    /// The current connection attempt; a result of any other is dropped.
    generation: u64,
}

#[derive(Clone)]
pub(crate) struct Gateway {
    connections: Arc<Mutex<BTreeMap<String, Connection>>>,
    /// Servers a save that couldn't finish left disconnected: their settings and secrets may
    /// not match, so nothing but a save connects them again. Kept in `run/held`, so turning
    /// a server off and on, or the daemon restarting, doesn't let one go.
    held: Arc<Mutex<BTreeSet<String>>>,
    notify: Notify,
    /// Finds local servers' commands, with the user's full `PATH`.
    env: Env,
}

/// Hears a server's notifications.
#[derive(Clone)]
struct Handler {
    name: String,
    gateway: Gateway,
}

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        let mut info = ClientConfig::default();
        info.client_info = Implementation::new("xolkit", env!("CARGO_PKG_VERSION"));
        info
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.gateway.refresh_tools(&self.name).await;
    }
}

impl Gateway {
    pub(crate) fn new(env: Env, notify: Notify) -> Gateway {
        let held = fs::read_to_string(env.run_dir().join("held"))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        Gateway {
            connections: Arc::new(Mutex::new(BTreeMap::new())),
            held: Arc::new(Mutex::new(held)),
            notify,
            env,
        }
    }

    fn log(&self, source: &str, message: impl Into<String>, error: bool) {
        (self.notify)(Notice::Log {
            source: source.to_owned(),
            message: message.into(),
            error,
        });
    }

    /// Connects to the servers in `wanted` that aren't connected yet, or whose config changed,
    /// and disconnects from the ones no longer wanted. Stopped servers stay stopped.
    pub(crate) fn sync(&self, wanted: Vec<Server>) {
        let mut start = Vec::new();
        let mut stopped = Vec::new();
        {
            let mut connections = self.connections.lock().unwrap();
            let names: Vec<String> = connections.keys().cloned().collect();
            for name in names {
                if !wanted.iter().any(|s| s.name == name) {
                    if let Some(old) = connections.remove(&name) {
                        stopped.extend(old.service);
                    }
                    self.log(&name, "disconnected: no longer on for any agent", false);
                }
            }
            for server in wanted {
                match connections.get_mut(&server.name) {
                    Some(c) if c.server.config == server.config => continue,
                    // A server stopped by hand stays stopped when edited; reconnecting starts
                    // it with the new settings.
                    Some(c) if c.state == ServerState::Stopped || self.is_held(&c.server.name) => {
                        c.server = server;
                        continue;
                    }
                    Some(c) => stopped.extend(c.service.clone()),
                    None if self.is_held(&server.name) => {
                        connections.insert(
                            server.name.clone(),
                            Connection {
                                server,
                                state: ServerState::Failed {
                                    message: HELD.to_owned(),
                                },
                                service: None,
                                tools: Vec::new(),
                                pid: None,
                                calls: 0,
                                latency_ms: None,
                                generation: next_attempt(),
                            },
                        );
                        continue;
                    }
                    None => {}
                }
                let generation = next_attempt();
                connections.insert(
                    server.name.clone(),
                    Connection {
                        server: server.clone(),
                        state: ServerState::Starting,
                        service: None,
                        tools: Vec::new(),
                        pid: None,
                        calls: 0,
                        latency_ms: None,
                        generation,
                    },
                );
                start.push((server, generation));
            }
        }
        for service in stopped {
            tokio::spawn(close(service));
        }
        for (server, generation) in start {
            self.spawn_connect(server, generation);
        }
        (self.notify)(Notice::ToolsChanged);
    }

    /// Reconnects to a server, whatever state it is in.
    pub(crate) fn restart(&self, name: &str) -> Result<()> {
        let (server, generation, old) = {
            let mut connections = self.connections.lock().unwrap();
            let connection = connections.get_mut(name).context("no such server")?;
            connection.generation = next_attempt();
            connection.state = ServerState::Starting;
            connection.tools.clear();
            connection.pid = None;
            (
                connection.server.clone(),
                connection.generation,
                connection.service.take(),
            )
        };
        if let Some(old) = old {
            tokio::spawn(close(old));
        }
        self.spawn_connect(server, generation);
        (self.notify)(Notice::ToolsChanged);
        Ok(())
    }

    pub(crate) fn stop(&self, name: &str) -> Result<()> {
        let old = {
            let mut connections = self.connections.lock().unwrap();
            let connection = connections.get_mut(name).context("no such server")?;
            connection.generation = next_attempt();
            connection.state = ServerState::Stopped;
            connection.tools.clear();
            connection.pid = None;
            connection.service.take()
        };
        if let Some(old) = old {
            tokio::spawn(close(old));
        }
        self.log(name, "stopped", false);
        (self.notify)(Notice::ToolsChanged);
        Ok(())
    }

    /// Disconnects `name` and holds it stopped while its settings and secrets change, cutting
    /// off any attempt under way. Returns whether it was held: `false` for a server unknown
    /// here or stopped by hand, which stays as it is. Release it with `restart`.
    pub(crate) fn hold(&self, name: &str) -> bool {
        let old = {
            let mut connections = self.connections.lock().unwrap();
            let Some(connection) = connections.get_mut(name) else {
                return false;
            };
            // One a save left disconnected is held again, even if stopped since.
            if connection.state == ServerState::Stopped && !self.is_held(name) {
                return false;
            }
            connection.generation = next_attempt();
            connection.state = ServerState::Stopped;
            connection.tools.clear();
            connection.pid = None;
            connection.service.take()
        };
        if let Some(old) = old {
            tokio::spawn(close(old));
        }
        true
    }

    /// Leaves `name` disconnected with `message`, held until a save releases it with
    /// `release`. Unlike one stopped by hand, the next save holds it again and connects it.
    pub(crate) fn fail(&self, name: &str, message: &str) {
        // Held here even if it can't be written down.
        {
            let mut held = self.held.lock().unwrap();
            if held.insert(name.to_owned())
                && let Err(e) = self.store(&held)
            {
                self.log(name, format!("{e:#}"), true);
            }
        }
        if let Some(connection) = self.connections.lock().unwrap().get_mut(name) {
            connection.state = ServerState::Failed {
                message: message.to_owned(),
            };
        }
        (self.notify)(Notice::ToolsChanged);
    }

    /// Holds `name` until `release`, here and in `run/held`; see `held`. On an error it isn't
    /// held at all.
    pub(crate) fn mark(&self, name: &str) -> Result<()> {
        let mut held = self.held.lock().unwrap();
        if !held.insert(name.to_owned()) {
            return Ok(());
        }
        let stored = self.store(&held);
        if stored.is_err() {
            held.remove(name);
        }
        stored
    }

    /// Lets a server held by `mark` or `fail` connect again.
    pub(crate) fn release(&self, name: &str) {
        let mut held = self.held.lock().unwrap();
        if held.remove(name)
            && let Err(e) = self.store(&held)
        {
            self.log(name, format!("{e:#}"), true);
        }
    }

    /// Writes the held servers to `run/held`, whole or not at all.
    fn store(&self, held: &BTreeSet<String>) -> Result<()> {
        let path = self.env.run_dir().join("held");
        let temp = path.with_extension("tmp");
        let text: String = held.iter().map(|n| format!("{n}\n")).collect();
        fs::create_dir_all(self.env.run_dir())
            .and_then(|()| fs::write(&temp, text))
            .and_then(|()| fs::rename(&temp, &path))
            .with_context(|| format!("could not write {}", path.display()))
    }

    /// Whether a save left `name` disconnected; see `held`.
    pub(crate) fn is_held(&self, name: &str) -> bool {
        self.held.lock().unwrap().contains(name)
    }

    /// Whether attempt `generation` is still the one wanted for `name`.
    fn current(&self, name: &str, generation: u64) -> bool {
        self.connections
            .lock()
            .unwrap()
            .get(name)
            .is_some_and(|c| c.generation == generation)
    }

    /// Disconnects from every server, for when the daemon exits.
    pub(crate) async fn close_all(&self) {
        let services: Vec<_> = {
            let mut connections = self.connections.lock().unwrap();
            connections
                .values_mut()
                .filter_map(|c| c.service.take())
                .collect()
        };
        for service in services {
            close(service).await;
        }
    }

    fn spawn_connect(&self, server: Server, generation: u64) {
        let gateway = self.clone();
        tokio::spawn(async move {
            let name = server.name.clone();
            let result =
                tokio::time::timeout(CONNECT_TIMEOUT, gateway.connect(&server, generation))
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("it didn't answer within a minute")));
            let mut connections = gateway.connections.lock().unwrap();
            let Some(connection) = connections.get_mut(&name) else {
                return;
            };
            if connection.generation != generation {
                return;
            }
            match result {
                Ok((service, tools, pid)) => {
                    gateway.log(&name, format!("connected, {} tools", tools.len()), false);
                    connection.state = ServerState::Running;
                    connection.service = Some(Arc::new(service));
                    connection.tools = tools;
                    connection.pid = pid;
                }
                Err(e) => {
                    let message = format!("{e:#}");
                    connection.state = match e.downcast_ref::<MissingSecret>() {
                        Some(MissingSecret(key)) => ServerState::NeedsSecret { key: key.clone() },
                        None => ServerState::Failed {
                            message: message.clone(),
                        },
                    };
                    gateway.log(&name, format!("could not connect: {message}"), true);
                }
            }
            drop(connections);
            (gateway.notify)(Notice::ToolsChanged);
        });
    }

    async fn connect(
        &self,
        server: &Server,
        generation: u64,
    ) -> Result<(RunningService<RoleClient, Handler>, Vec<Tool>, Option<u32>)> {
        let handler = Handler {
            name: server.name.clone(),
            gateway: self.clone(),
        };
        // The keyring can block, waiting for the user to unlock it, and finding a launcher may
        // ask the login shell; keep both off the workers.
        let (config, env) = {
            let (name, config, env) =
                (server.name.clone(), server.config.clone(), self.env.clone());
            tokio::task::spawn_blocking(move || {
                let config = with_secrets(&name, &config)?;
                let env = match (&config.url, &config.command) {
                    (None, Some(program)) => launcher_env(&env, program),
                    _ => env,
                };
                anyhow::Ok((config, env))
            })
            .await??
        };
        // Settings and secrets may have changed while they were read; connect only if this
        // attempt is still the one wanted, so a secret never goes to an address it wasn't for.
        anyhow::ensure!(
            self.current(&server.name, generation),
            "superseded by a newer connection"
        );
        let (service, pid) = match &config.url {
            Some(url) => {
                let mut headers = HashMap::new();
                for (key, value) in &config.headers {
                    headers.insert(
                        http::HeaderName::try_from(key.as_str())
                            .with_context(|| format!("bad header name {key}"))?,
                        http::HeaderValue::try_from(value.as_str())
                            .with_context(|| format!("bad value for header {key}"))?,
                    );
                }
                let transport = StreamableHttpClientTransport::from_config(
                    StreamableHttpClientTransportConfig::with_uri(url.as_str())
                        .custom_headers(headers),
                );
                (handler.serve(transport).await?, None)
            }
            None => {
                // Found the way a shell finds it, so `npx` (npx.cmd on Windows) works.
                let name = config.command.as_deref().unwrap_or_default();
                let mut command = tokio::process::Command::new(env.find(name));
                if let Some(path) = &env.path {
                    command.env("PATH", path);
                }
                command.args(&config.args).envs(&config.env);
                // From home unless the server says otherwise: not from whichever project's
                // session happened to start the daemon, which could put its own `npx` first.
                command.current_dir(config.cwd.as_deref().unwrap_or(&self.env.home));
                #[cfg(windows)]
                command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
                let (transport, stderr) = TokioChildProcess::builder(command)
                    .stderr(Stdio::piped())
                    .spawn()
                    .with_context(|| {
                        format!(
                            "could not start `{}`",
                            config.command.as_deref().unwrap_or("")
                        )
                    })?;
                let pid = transport.id();
                if let Some(stderr) = stderr {
                    let gateway = self.clone();
                    let name = server.name.clone();
                    tokio::spawn(async move {
                        // Lines past 4 KB are cut, so a server that never ends a line can't
                        // grow one without end.
                        let mut reader = BufReader::new(stderr);
                        let mut line = Vec::new();
                        loop {
                            line.clear();
                            match (&mut reader).take(4096).read_until(b'\n', &mut line).await {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {}
                            }
                            if !line.ends_with(b"\n") {
                                // Skip the rest of a long line.
                                let mut rest = Vec::new();
                                while let Ok(1..) =
                                    (&mut reader).take(4096).read_until(b'\n', &mut rest).await
                                {
                                    if rest.ends_with(b"\n") {
                                        break;
                                    }
                                    rest.clear();
                                }
                            }
                            let text = String::from_utf8_lossy(&line);
                            gateway.log(&name, text.trim_end().to_owned(), false);
                        }
                    });
                }
                (handler.serve(transport).await?, pid)
            }
        };
        let tools = service.list_all_tools().await?;
        Ok((service, tools, pid))
    }

    async fn refresh_tools(&self, name: &str) {
        let (service, generation) = {
            let connections = self.connections.lock().unwrap();
            match connections.get(name) {
                Some(c) => (c.service.clone(), c.generation),
                None => return,
            }
        };
        let Some(service) = service else {
            return;
        };
        match service.list_all_tools().await {
            Ok(tools) => {
                // Only for the connection that asked: it may have been stopped or replaced since.
                if let Some(c) = self.connections.lock().unwrap().get_mut(name)
                    && c.generation == generation
                {
                    c.tools = tools;
                }
                (self.notify)(Notice::ToolsChanged);
            }
            Err(e) => self.log(name, format!("could not list tools: {e}"), true),
        }
    }

    /// Notices local servers that exited and remote ones whose connection ended.
    pub(crate) fn reap(&self) {
        let mut ended = Vec::new();
        {
            let mut connections = self.connections.lock().unwrap();
            for (name, connection) in connections.iter_mut() {
                if connection.service.as_ref().is_some_and(|s| s.is_closed()) {
                    connection.service = None;
                    connection.tools.clear();
                    connection.pid = None;
                    connection.state = ServerState::Failed {
                        message: "it stopped; reconnect to start it again".to_owned(),
                    };
                    ended.push(name.clone());
                }
            }
        }
        for name in &ended {
            self.log(name, "stopped unexpectedly", true);
        }
        if !ended.is_empty() {
            (self.notify)(Notice::ToolsChanged);
        }
    }

    /// The tools of server `name`, if it is connected.
    pub(crate) fn tools(&self, name: &str) -> Vec<Tool> {
        let connections = self.connections.lock().unwrap();
        connections
            .get(name)
            .map(|c| c.tools.clone())
            .unwrap_or_default()
    }

    /// Calls `tool` on server `name`.
    pub(crate) async fn call(
        &self,
        name: &str,
        tool: &str,
        arguments: Option<rmcp::model::JsonObject>,
    ) -> Result<CallToolResult> {
        let service = {
            let connections = self.connections.lock().unwrap();
            let connection = connections.get(name).context("no such server")?;
            match (&connection.service, &connection.state) {
                (Some(service), _) => service.clone(),
                (None, ServerState::NeedsSecret { key }) => {
                    bail!("the server {name} needs {key}; save it in xolkit")
                }
                (None, state) => bail!("the server {name} is not connected ({state:?})"),
            }
        };
        let mut params = CallToolRequestParams::new(tool.to_owned());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        let started = Instant::now();
        let result = tokio::time::timeout(CALL_TIMEOUT, service.call_tool(params))
            .await
            .context("the call took too long")?;
        if let Some(c) = self.connections.lock().unwrap().get_mut(name) {
            c.calls += 1;
            c.latency_ms = Some(started.elapsed().as_millis() as u64);
        }
        result.with_context(|| format!("{name} failed"))
    }

    pub(crate) fn status(&self) -> Vec<ServerStatus> {
        let connections = self.connections.lock().unwrap();
        connections
            .values()
            .map(|c| ServerStatus {
                name: c.server.name.clone(),
                plugin: c.server.plugin.clone(),
                remote: c.server.config.is_remote(),
                state: c.state.clone(),
                pid: c.pid,
                tools: c.tools.len(),
                calls: c.calls,
                latency_ms: c.latency_ms,
            })
            .collect()
    }
}

/// A number no connection attempt has had, so a late result of an old attempt (even of a server
/// removed and added again) is never taken for the current one.
fn next_attempt() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The environment to start `program` in: the daemon's own, or, when its `PATH` misses the
/// program, the `PATH` as it is now, for a launcher installed since the daemon started. A
/// program given by path is found by its path, so no fresh `PATH` can help it.
fn launcher_env(env: &Env, program: &str) -> Env {
    if env.has_tool(program) || program.contains(['/', '\\']) {
        return env.clone();
    }
    env.with_fresh_path()
        .filter(|fresh| fresh.has_tool(program))
        .unwrap_or_else(|| env.clone())
}

async fn close(service: Arc<RunningService<RoleClient, Handler>>) {
    // The last holder closes it; a call still running keeps it open until it returns.
    if let Ok(service) = Arc::try_unwrap(service) {
        let _ = service.cancel().await;
    }
}

#[derive(Debug)]
struct MissingSecret(String);

impl std::fmt::Display for MissingSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} isn't saved yet", self.0)
    }
}

impl std::error::Error for MissingSecret {}

/// `config` with the values of its secrets filled in from the keyring.
fn with_secrets(name: &str, config: &ServerConfig) -> Result<ServerConfig> {
    let mut config = config.clone();
    for key in config.secrets.clone() {
        let value =
            get_secret(&secret_key(name, &key))?.ok_or_else(|| MissingSecret(key.clone()))?;
        if config.is_remote() {
            config.headers.insert(key, value);
        } else {
            config.env.insert(key, value);
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use xolkit_core::ServerConfig;

    fn state(gateway: &Gateway) -> ServerState {
        gateway.status().remove(0).state
    }

    #[tokio::test]
    async fn a_server_a_save_left_disconnected_waits_for_a_save() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let gateway = Gateway::new(env.clone(), Arc::new(|_| {}));
        let server = |arg: &str| Server {
            name: "a".into(),
            config: ServerConfig {
                command: Some("xolkit-test-missing-command".into()),
                args: vec![arg.into()],
                ..ServerConfig::default()
            },
            plugin: None,
        };
        gateway.sync(vec![server("1")]);
        assert!(gateway.hold("a"));
        gateway.fail("a", "save again");
        assert!(gateway.is_held("a"));
        // New settings alone don't connect it, nor does stopping it let it go.
        gateway.sync(vec![server("2")]);
        assert!(matches!(state(&gateway), ServerState::Failed { .. }));
        gateway.stop("a").unwrap();
        assert!(gateway.hold("a"));
        // Nor turning it off and on, nor the daemon starting again.
        gateway.sync(vec![]);
        let gateway = Gateway::new(env, Arc::new(|_| {}));
        assert!(gateway.is_held("a"));
        gateway.sync(vec![server("3")]);
        assert!(matches!(state(&gateway), ServerState::Failed { .. }));
        // A save lets it go, and connects it with the settings synced last.
        assert!(gateway.hold("a"));
        gateway.release("a");
        gateway.restart("a").unwrap();
        assert!(!gateway.is_held("a"));
        assert_eq!(state(&gateway), ServerState::Starting);
        assert_eq!(gateway.connections.lock().unwrap()["a"].server, server("3"));
    }
}
