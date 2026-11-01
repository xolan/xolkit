//! Connections to the MCP servers xolkit passes on: local ones it starts, remote ones over
//! streamable HTTP. One connection per server, shared by every session.

use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig, Implementation, Tool};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use tokio::io::{AsyncBufReadExt, BufReader};
use xolkit_core::{Env, Server, ServerConfig, get_secret, secret_key};

use crate::protocol::{ServerState, ServerStatus};

/// How long a server gets to start and list its tools.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long one tool call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

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
    /// Bumped on every (re)connect, so a late result of an older attempt is dropped.
    generation: u64,
}

#[derive(Clone)]
pub(crate) struct Gateway {
    connections: Arc<Mutex<BTreeMap<String, Connection>>>,
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
        Gateway {
            connections: Arc::new(Mutex::new(BTreeMap::new())),
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
                match connections.get(&server.name) {
                    Some(c) if c.server.config == server.config => continue,
                    Some(c) => stopped.extend(c.service.clone()),
                    None => {}
                }
                let generation = connections
                    .get(&server.name)
                    .map_or(0, |c| c.generation + 1);
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
            connection.generation += 1;
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
            connection.generation += 1;
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
            let result = tokio::time::timeout(CONNECT_TIMEOUT, gateway.connect(&server))
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
    ) -> Result<(RunningService<RoleClient, Handler>, Vec<Tool>, Option<u32>)> {
        let handler = Handler {
            name: server.name.clone(),
            gateway: self.clone(),
        };
        // The keyring can block, waiting for the user to unlock it; keep that off the workers.
        let config = {
            let (name, config) = (server.name.clone(), server.config.clone());
            tokio::task::spawn_blocking(move || with_secrets(&name, &config)).await??
        };
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
                let program = self.env.find(config.command.as_deref().unwrap_or_default());
                let mut command = tokio::process::Command::new(program);
                if let Some(path) = &self.env.path {
                    command.env("PATH", path);
                }
                command.args(&config.args).envs(&config.env);
                if let Some(cwd) = &config.cwd {
                    command.current_dir(cwd);
                }
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
                        let mut lines = BufReader::new(stderr).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            gateway.log(&name, line, false);
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
        let service = {
            let connections = self.connections.lock().unwrap();
            connections.get(name).and_then(|c| c.service.clone())
        };
        let Some(service) = service else {
            return;
        };
        match service.list_all_tools().await {
            Ok(tools) => {
                if let Some(c) = self.connections.lock().unwrap().get_mut(name) {
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
