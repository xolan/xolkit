//! The daemon: one per user, started by the first shim or by the app. It serves agent sessions,
//! keeps the connections to MCP servers, runs changes for the app, and exits a few minutes
//! after the last session and the app are gone.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::traits::tokio::Listener as _;
use rmcp::ServiceExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::sync::watch;
use xolkit_core::{
    Agent, Catalogue, Env, Library, Provider, catalogue, execute, router_skill, set_secret,
    write_router,
};

use crate::gateway::{Gateway, Notice};
use crate::ipc;
use crate::protocol::{Event, Hello, Request, Response, SessionStatus, Status, now};
use crate::session::Session;

/// Events kept for the Connections page.
const MAX_EVENTS: usize = 500;
/// How often the daemon looks for changes made to `~/.xolkit` by hand.
const WATCH_INTERVAL: Duration = Duration::from_secs(2);
/// Longest line a control connection may send: a plan of actions with a profile's settings.
const MAX_CONTROL_LINE: usize = 16 * 1024 * 1024;

pub(crate) struct Daemon {
    env: Env,
    library: RwLock<Arc<Library>>,
    /// Bumped whenever what agents get may have changed.
    changes: watch::Sender<u64>,
    gateway: OnceLock<Gateway>,
    sessions: Mutex<BTreeMap<u64, (SessionStatus, Option<tokio::task::AbortHandle>)>>,
    next_session: AtomicU64,
    events: Mutex<VecDeque<Event>>,
    /// Open control connections: the app, while it runs.
    controls: AtomicUsize,
    last_active: Mutex<Instant>,
    started: u64,
    shutdown: tokio::sync::Notify,
    log: Mutex<Option<std::fs::File>>,
    /// Serializes changes, so two plans never run at once.
    executing: tokio::sync::Mutex<()>,
}

impl Daemon {
    fn new(env: Env) -> Arc<Daemon> {
        // A file that doesn't read starts the daemon empty, not at all: sessions keep working,
        // the app can say what is wrong, and the watcher picks up the fix.
        let (library, problem) = match Library::load(&env) {
            Ok(library) => (library, None),
            Err(e) => (
                Library::default(),
                Some(format!("could not read the library: {e:#}")),
            ),
        };
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(env.run_dir().join("daemon.log"))
            .ok();
        let daemon = Arc::new(Daemon {
            env,
            library: RwLock::new(Arc::new(library)),
            changes: watch::channel(0).0,
            gateway: OnceLock::new(),
            sessions: Mutex::new(BTreeMap::new()),
            next_session: AtomicU64::new(1),
            events: Mutex::new(VecDeque::new()),
            controls: AtomicUsize::new(0),
            last_active: Mutex::new(Instant::now()),
            started: now(),
            shutdown: tokio::sync::Notify::new(),
            log: Mutex::new(log),
            executing: tokio::sync::Mutex::new(()),
        });
        let weak = Arc::downgrade(&daemon);
        let gateway = Gateway::new(
            daemon.env.clone(),
            Arc::new(move |notice| {
                let Some(daemon) = weak.upgrade() else {
                    return;
                };
                match notice {
                    Notice::ToolsChanged => daemon.changed(),
                    Notice::Log {
                        source,
                        message,
                        error,
                    } => daemon.event(&source, message, error),
                }
            }),
        );
        let _ = daemon.gateway.set(gateway);
        if let Some(problem) = problem {
            daemon.event("daemon", problem, true);
        }
        daemon
    }

    pub(crate) fn gateway(&self) -> &Gateway {
        self.gateway.get().expect("the gateway is set at start")
    }

    pub(crate) fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn changed(&self) {
        self.changes.send_modify(|n| *n += 1);
    }

    fn library(&self) -> Arc<Library> {
        self.library.read().unwrap().clone()
    }

    pub(crate) fn catalogue(&self, agent: Agent) -> Catalogue {
        catalogue(&self.library(), agent)
    }

    pub(crate) fn event(&self, source: &str, message: impl Into<String>, error: bool) {
        let event = Event {
            time: now(),
            source: source.to_owned(),
            message: message.into(),
            error,
        };
        if let Some(log) = self.log.lock().unwrap().as_mut() {
            let mark = if error { "error" } else { "info" };
            let _ = writeln!(
                log,
                "{} {mark} [{}] {}",
                event.time, event.source, event.message
            );
        }
        let mut events = self.events.lock().unwrap();
        events.push_back(event);
        while events.len() > MAX_EVENTS {
            events.pop_front();
        }
    }

    pub(crate) fn session_client(&self, id: u64, client: String) {
        if let Some((status, _)) = self.sessions.lock().unwrap().get_mut(&id)
            && status.client.as_ref() != Some(&client)
        {
            status.client = Some(client);
        }
    }

    pub(crate) fn session_call(&self, id: u64, what: &str) {
        *self.last_active.lock().unwrap() = Instant::now();
        if let Some((status, _)) = self.sessions.lock().unwrap().get_mut(&id) {
            status.calls += 1;
            status.last_call = Some(now());
        }
        self.event(&format!("session {id}"), what, false);
    }

    pub(crate) fn session_error(&self, id: u64, message: &str) {
        if let Some((status, _)) = self.sessions.lock().unwrap().get_mut(&id) {
            status.errors += 1;
        }
        self.event(&format!("session {id}"), message, true);
    }

    /// Reads `~/.xolkit` again, and brings everything that follows from it up to date.
    fn reload(&self) {
        match Library::load(&self.env) {
            Ok(library) => {
                let same = **self.library.read().unwrap() == library;
                *self.library.write().unwrap() = Arc::new(library);
                if !same {
                    self.apply();
                }
            }
            Err(e) => self.event("daemon", format!("could not read the library: {e:#}"), true),
        }
    }

    /// Connects to the servers some agent gets, writes the router skills, and tells sessions.
    fn apply(&self) {
        let library = self.library();
        let mut wanted = Vec::new();
        for agent in Agent::ALL {
            for server in catalogue(&library, agent).servers {
                if !wanted
                    .iter()
                    .any(|s: &xolkit_core::Server| s.name == server.name)
                {
                    wanted.push(server);
                }
            }
        }
        self.gateway().sync(wanted);
        for provider in Provider::ALL {
            let on = library.settings.router.contains(&provider);
            let skill = on.then(|| router_skill(&catalogue(&library, Agent::from(provider))));
            if !on && !router_is_ours(&self.env, provider) {
                continue;
            }
            if let Err(e) = write_router(&self.env, provider, skill.as_deref()) {
                self.event(
                    "daemon",
                    format!("router skill for {}: {e:#}", provider.name()),
                    true,
                );
            }
        }
        self.changed();
    }

    fn status(&self) -> Status {
        Status {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            started: self.started,
            idle_minutes: self.library().settings.idle_minutes,
            sessions: self
                .sessions
                .lock()
                .unwrap()
                .values()
                .map(|(s, _)| s.clone())
                .collect(),
            servers: self.gateway().status(),
            events: self.events.lock().unwrap().iter().cloned().collect(),
        }
    }

    async fn handle(self: &Arc<Self>, request: Request) -> Response {
        let result = match request {
            Request::Status => return Response::Status(Box::new(self.status())),
            Request::Execute { actions } => {
                let _running = self.executing.lock().await;
                let env = self.env.clone();
                for action in &actions {
                    self.event("app", action.describe(&env), false);
                }
                let done = tokio::task::spawn_blocking(move || execute(&env, &actions))
                    .await
                    .context("the change stopped unexpectedly")
                    .and_then(|r| r);
                self.reload();
                done
            }
            Request::SetSecret { key, value } => {
                let done = tokio::task::spawn_blocking(move || set_secret(&key, value.as_deref()))
                    .await
                    .context("the keyring stopped unexpectedly")
                    .and_then(|r| r);
                // A server waiting for the secret can try again now.
                let waiting: Vec<String> = self
                    .gateway()
                    .status()
                    .into_iter()
                    .filter(|s| matches!(s.state, crate::protocol::ServerState::NeedsSecret { .. }))
                    .map(|s| s.name)
                    .collect();
                for name in waiting {
                    let _ = self.gateway().restart(&name);
                }
                done
            }
            Request::RestartServer { name } => self.gateway().restart(&name),
            Request::StopServer { name } => self.gateway().stop(&name),
            Request::Disconnect { session } => {
                let removed = self.sessions.lock().unwrap().remove(&session);
                match removed {
                    Some((_, abort)) => {
                        if let Some(abort) = abort {
                            abort.abort();
                        }
                        self.event(
                            &format!("session {session}"),
                            "disconnected by the app",
                            false,
                        );
                        Ok(())
                    }
                    None => Err(anyhow::anyhow!("no such session")),
                }
            }
            Request::Shutdown => {
                self.shutdown.notify_one();
                Ok(())
            }
        };
        match result {
            Ok(()) => Response::Ok,
            Err(e) => {
                let message = format!("{e:#}");
                self.event("daemon", &message, true);
                Response::Error { message }
            }
        }
    }

    async fn serve_connection(self: Arc<Self>, stream: Stream) -> Result<()> {
        let (recv, send) = tokio::io::split(stream);
        let mut reader = BufReader::new(recv);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        let hello: Hello = serde_json::from_str(&line).context("a connection said no hello")?;
        match hello {
            Hello::Session { agent, pid, cwd } => {
                self.serve_session(reader, send, agent, pid, cwd).await
            }
            Hello::Control => {
                self.controls.fetch_add(1, Ordering::SeqCst);
                let result = self.serve_control(reader, send).await;
                self.controls.fetch_sub(1, Ordering::SeqCst);
                *self.last_active.lock().unwrap() = Instant::now();
                result
            }
        }
    }

    async fn serve_session(
        self: Arc<Self>,
        reader: BufReader<ReadHalf<Stream>>,
        writer: WriteHalf<Stream>,
        agent: Option<Agent>,
        pid: u32,
        cwd: Option<PathBuf>,
    ) -> Result<()> {
        let id = self.next_session.fetch_add(1, Ordering::SeqCst);
        let agent = agent.unwrap_or(Agent::Other);
        let status = SessionStatus {
            id,
            agent,
            client: None,
            pid,
            cwd: cwd.clone(),
            connected: now(),
            last_call: None,
            calls: 0,
            errors: 0,
        };
        self.sessions.lock().unwrap().insert(id, (status, None));
        let source = format!("session {id}");
        let place = cwd
            .map(|c| format!(" in {}", self.env.tilde(&c)))
            .unwrap_or_default();
        self.event(&source, format!("{} connected{place}", agent.name()), false);
        let session = Session {
            daemon: self.clone(),
            id,
            agent,
        };
        let task = tokio::spawn(async move {
            match session.serve((reader, writer)).await {
                Ok(running) => {
                    let _ = running.waiting().await;
                }
                Err(e) => return Err(anyhow::Error::from(e)),
            }
            Ok(())
        });
        if let Some((_, abort)) = self.sessions.lock().unwrap().get_mut(&id) {
            *abort = Some(task.abort_handle());
        }
        let result = task.await;
        self.sessions.lock().unwrap().remove(&id);
        *self.last_active.lock().unwrap() = Instant::now();
        match result {
            Ok(Err(e)) => self.event(&source, format!("ended: {e:#}"), true),
            _ => self.event(&source, "disconnected", false),
        }
        Ok(())
    }

    async fn serve_control(
        self: &Arc<Self>,
        mut reader: BufReader<ReadHalf<Stream>>,
        mut writer: WriteHalf<Stream>,
    ) -> Result<()> {
        let mut line = String::new();
        loop {
            line.clear();
            let read = (&mut reader)
                .take(MAX_CONTROL_LINE as u64)
                .read_line(&mut line)
                .await?;
            if read == 0 {
                return Ok(());
            }
            let response = match serde_json::from_str::<Request>(&line) {
                Ok(request) => self.handle(request).await,
                Err(e) => Response::Error {
                    message: format!("not a request: {e}"),
                },
            };
            let mut out = serde_json::to_string(&response)?;
            out.push('\n');
            writer.write_all(out.as_bytes()).await?;
            writer.flush().await?;
        }
    }

    fn idle(&self) -> bool {
        let minutes = self.library().settings.idle_minutes;
        self.sessions.lock().unwrap().is_empty()
            && self.controls.load(Ordering::SeqCst) == 0
            && self.last_active.lock().unwrap().elapsed() > Duration::from_secs(minutes * 60)
    }
}

/// Keeps the log from growing forever: past 5 MB, it moves to `daemon.log.old` at start.
fn rotate_log(env: &Env) {
    let log = env.run_dir().join("daemon.log");
    if std::fs::metadata(&log).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
        let _ = std::fs::rename(&log, env.run_dir().join("daemon.log.old"));
    }
}

/// Whether the router skill in `provider`'s folder is one xolkit wrote, so it may be removed.
fn router_is_ours(env: &Env, provider: Provider) -> bool {
    let dir = match provider {
        Provider::Claude => env.claude_skills(),
        Provider::Codex => env.codex_skills(),
    };
    std::fs::read_to_string(dir.join(xolkit_core::XOLKIT).join("SKILL.md"))
        .is_ok_and(|text| text.contains("This skill is written by xolkit"))
}

/// The modification times of everything the library is read from.
fn fingerprint(env: &Env) -> Vec<Option<SystemTime>> {
    let mtime = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let mut times = vec![
        mtime(&env.settings_file()),
        mtime(&env.lock_file()),
        mtime(&env.servers_file()),
        mtime(&env.skills_dir()),
        mtime(&env.plugins_dir()),
    ];
    let folders = |dir: &Path| -> Vec<PathBuf> {
        let mut inner: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
        inner.sort();
        inner
    };
    for skill in folders(&env.skills_dir()) {
        times.extend([mtime(&skill), mtime(&skill.join("SKILL.md"))]);
    }
    // What a plugin is read from: its manifest, servers, commands and skills.
    for plugin in folders(&env.plugins_dir()) {
        for part in [
            ".claude-plugin/plugin.json",
            ".mcp.json",
            "commands",
            "skills",
        ] {
            times.push(mtime(&plugin.join(part)));
        }
        for skill in folders(&plugin.join("skills")) {
            times.push(mtime(&skill.join("SKILL.md")));
        }
    }
    times
}

/// Runs the daemon until it is told to stop or has been idle long enough.
pub async fn run(env: Env) -> Result<()> {
    let (listener, instance) = ipc::listen(&env)?;
    rotate_log(&env);
    let daemon = Daemon::new(env.clone());
    let _ = std::fs::write(
        env.run_dir().join("daemon.pid"),
        std::process::id().to_string(),
    );
    daemon.event(
        "daemon",
        format!("started, version {}", env!("CARGO_PKG_VERSION")),
        false,
    );
    daemon.apply();

    let watcher = {
        let daemon = daemon.clone();
        let env = env.clone();
        tokio::spawn(async move {
            let mut last = fingerprint(&env);
            loop {
                tokio::time::sleep(WATCH_INTERVAL).await;
                daemon.gateway().reap();
                // Mid-plan, the files are half changed; the plan reloads when it ends.
                let Ok(_idle) = daemon.executing.try_lock() else {
                    continue;
                };
                let now = fingerprint(&env);
                if now != last {
                    last = now;
                    daemon.reload();
                }
                if daemon.idle() {
                    daemon.event("daemon", "idle; exiting", false);
                    daemon.shutdown.notify_one();
                }
            }
        })
    };

    loop {
        tokio::select! {
            () = daemon.shutdown.notified() => break,
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    let daemon = daemon.clone();
                    tokio::spawn(async move {
                        if let Err(e) = daemon.clone().serve_connection(stream).await {
                            daemon.event("daemon", format!("connection: {e:#}"), true);
                        }
                    });
                }
                Err(e) => daemon.event("daemon", format!("could not accept: {e}"), true),
            },
        }
    }
    watcher.abort();
    daemon.event("daemon", "stopped", false);
    daemon.gateway().close_all().await;
    ipc::remove_socket(&env, &instance);
    let _ = std::fs::remove_file(env.run_dir().join("daemon.pid"));
    Ok(())
}
