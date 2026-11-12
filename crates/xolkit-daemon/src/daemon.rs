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
    Agent, Catalogue, Env, Library, Provider, catalogue, execute, router_is_ours, router_skill,
    set_secret, write_router,
};

use crate::gateway::{Gateway, Notice};
use crate::ipc;
use crate::protocol::{Event, Hello, Parent, Request, Response, SessionStatus, Status, now};
use crate::session::{SEPARATOR, Session};

/// Most files of one plugin the watcher looks at.
const MAX_WATCHED: usize = 2000;
/// Largest `daemon.log` before it starts over.
const MAX_LOG: u64 = 5 * 1024 * 1024;
/// Events kept for the Connections page.
const MAX_EVENTS: usize = 500;
/// How often the daemon looks for changes made to `~/.xolkit` by hand.
const WATCH_INTERVAL: Duration = Duration::from_secs(2);
/// Longest line a control connection may send: a plan of actions with a profile's settings.
const MAX_CONTROL_LINE: usize = 16 * 1024 * 1024;
/// Longest a stopping daemon waits for a refused app connection's request.
const REFUSED_READ: Duration = Duration::from_secs(5);
/// How long the daemon waits after a session ends before it asks whether the shim still runs:
/// one that ends because its agent did takes a moment to exit.
const SHIM_EXIT: Duration = Duration::from_secs(1);

/// A connection the daemon has accepted and not yet taken on or turned away. A stopping daemon
/// waits for these, so none is cut off before it hears it should go to the next daemon.
struct Handshake(Arc<Daemon>);

impl Drop for Handshake {
    fn drop(&mut self) {
        self.0.handshaking.fetch_sub(1, Ordering::SeqCst);
    }
}

/// How long finding a server's sign-in and registering with it may take.
const SIGN_IN_SETUP: Duration = Duration::from_secs(60);

pub(crate) struct Daemon {
    env: Env,
    library: RwLock<Arc<Library>>,
    /// Bumped whenever what agents get may have changed.
    changes: watch::Sender<u64>,
    gateway: OnceLock<Gateway>,
    /// Connected sessions, and idle ones whose shim still runs (`idle`, with no task).
    sessions: Mutex<BTreeMap<u64, (SessionStatus, Option<tokio::task::AbortHandle>)>>,
    next_session: AtomicU64,
    events: Mutex<VecDeque<Event>>,
    /// Open control connections: the app, while it runs.
    controls: AtomicUsize,
    last_active: Mutex<Instant>,
    started: u64,
    shutdown: tokio::sync::Notify,
    /// Set once the daemon has decided to exit; it takes no more sessions.
    stopping: std::sync::atomic::AtomicBool,
    /// Connections accepted and not yet taken on or turned away.
    handshaking: AtomicUsize,
    log: Mutex<Option<std::fs::File>>,
    /// Serializes changes, so two plans never run at once.
    executing: tokio::sync::Mutex<()>,
    /// Whether the library last failed to read: until it reads again, the daemon serves what it
    /// has and leaves the router skills and server connections alone.
    broken: std::sync::atomic::AtomicBool,
    /// Tools left out for their names, already logged once.
    left_out: Mutex<std::collections::HashSet<String>>,
    /// Sign-ins waiting for the browser, by server.
    sign_ins: Mutex<std::collections::HashMap<String, tokio::task::AbortHandle>>,
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
        let log = rotate_log(&env);
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
            stopping: std::sync::atomic::AtomicBool::new(false),
            handshaking: AtomicUsize::new(0),
            log: Mutex::new(log),
            executing: tokio::sync::Mutex::new(()),
            broken: std::sync::atomic::AtomicBool::new(problem.is_some()),
            left_out: Mutex::new(std::collections::HashSet::new()),
            sign_ins: Mutex::new(std::collections::HashMap::new()),
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
        {
            let mut log = self.log.lock().unwrap();
            // Past the size cap, start a new log, keeping the last one as daemon.log.old.
            if log
                .as_ref()
                .and_then(|f| f.metadata().ok())
                .is_some_and(|m| m.len() > MAX_LOG)
            {
                *log = rotate_log(&self.env);
            }
            if let Some(log) = log.as_mut() {
                let mark = if error { "error" } else { "info" };
                let _ = writeln!(
                    log,
                    "{} {mark} [{}] {}",
                    event.time, event.source, event.message
                );
            }
        }
        let mut events = self.events.lock().unwrap();
        events.push_back(event);
        while events.len() > MAX_EVENTS {
            events.pop_front();
        }
    }

    /// Logs that a server's tool was left out, once per tool and reason.
    pub(crate) fn left_out(&self, server: &str, tool: &str, why: &crate::session::LeftOut) {
        let key = format!("{server}{SEPARATOR}{tool}: {why}");
        if self.left_out.lock().unwrap().insert(key) {
            self.event(server, format!("left out the tool {tool}: {why}"), true);
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
    /// Returns whether it could be read.
    fn reload(&self) -> bool {
        match Library::load(&self.env) {
            Ok(library) => {
                let was_broken = self.broken.swap(false, Ordering::SeqCst);
                let same = !was_broken && **self.library.read().unwrap() == library;
                *self.library.write().unwrap() = Arc::new(library);
                if !same {
                    self.apply();
                }
                true
            }
            Err(e) => {
                // Once per breakage, not every time the watcher looks.
                if !self.broken.swap(true, Ordering::SeqCst) {
                    self.event("daemon", format!("could not read the library: {e:#}"), true);
                }
                false
            }
        }
    }

    /// Connects to the servers some agent gets, writes the router skills, and tells sessions.
    fn apply(&self) {
        if self.broken.load(Ordering::SeqCst) {
            return;
        }
        let library = self.library();
        for name in &library.ignored_servers {
            // Once per name, like tools left out.
            if self
                .left_out
                .lock()
                .unwrap()
                .insert(format!("server {name}"))
            {
                self.event(
                    "daemon",
                    format!(
                        "left out the server {name}: use letters, digits, - and _, without __ \
                         or a _ at either end"
                    ),
                    true,
                );
            }
        }
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
            session_idle_minutes: Some(self.library().settings.session_idle_minutes),
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
                // Saved, but if the library can't be read, nothing of it takes effect; say so.
                done.and_then(|()| match Library::load(&self.env) {
                    Ok(_) => Ok(()),
                    Err(e) => Err(e.context("saved, but the library can't be read")),
                })
            }
            Request::SetSecret {
                key,
                value,
                reconnect,
            } => {
                // Not while a server's settings and secrets change together.
                let _running = self.executing.lock().await;
                let saved = key.clone();
                let done = tokio::task::spawn_blocking(move || set_secret(&key, value.as_deref()))
                    .await
                    .context("the keyring stopped unexpectedly")
                    .and_then(|r| r);
                // Servers using the secret connect again with it, whether they waited for it,
                // failed with an old one or run with one that just changed. Stopped ones stay so,
                // and so do ones a save left disconnected.
                let using: Vec<String> = self
                    .gateway()
                    .status()
                    .into_iter()
                    .filter(|s| saved.starts_with(&format!("server/{}/", s.name)))
                    .filter(|s| s.state != crate::protocol::ServerState::Stopped)
                    .filter(|s| !self.gateway().is_held(&s.name))
                    .map(|s| s.name)
                    .collect();
                // Not with settings older than the library on disk, while it can't be read.
                if done.is_ok() && reconnect && !self.broken.load(Ordering::SeqCst) {
                    for name in using {
                        let _ = self.gateway().restart(&name);
                    }
                }
                done
            }
            Request::RestartServer { name } => self.restart_server(&name).await,
            Request::SignIn { name } => {
                return match self.sign_in(name).await {
                    Ok(page) => Response::SignIn { page },
                    Err(e) => {
                        let message = format!("{e:#}");
                        self.event("daemon", &message, true);
                        Response::Error { message }
                    }
                };
            }
            Request::SignOut { name } => {
                if let Some(waiting) = self.sign_ins.lock().unwrap().remove(&name) {
                    waiting.abort();
                }
                match self.gateway().sign_out(&name).await {
                    Ok(()) => {
                        self.event(&name, "signed out", false);
                        // Off the tokens it was connected with.
                        self.reconnect_if_on(&name).await
                    }
                    Err(e) => Err(e),
                }
            }
            Request::SaveServer {
                name,
                secrets,
                actions,
            } => self.save_server(name, secrets, actions).await,
            Request::StopServer { name } => {
                let _running = self.executing.lock().await;
                self.gateway().stop(&name)
            }
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
                self.stopping.store(true, Ordering::SeqCst);
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

    /// Reconnects to server `name`, unless the library can't be read or a save holds it.
    async fn restart_server(&self, name: &str) -> Result<()> {
        self.restart(name, false).await
    }

    /// Reconnects to server `name` after its tokens changed. One stopped by hand stays so, one
    /// no agent has on has nothing to reconnect, and one a save holds, or any while the library
    /// can't be read, connects once that is put right.
    async fn reconnect_if_on(&self, name: &str) -> Result<()> {
        self.restart(name, true).await
    }

    async fn restart(&self, name: &str, only_if_on: bool) -> Result<()> {
        // Under the lock, so a server held while it is saved isn't started halfway, and one
        // stopped meanwhile is seen stopped.
        let _running = self.executing.lock().await;
        if only_if_on {
            let on = self
                .gateway()
                .status()
                .iter()
                .any(|s| s.name == name && s.state != crate::protocol::ServerState::Stopped);
            if !on || self.gateway().is_held(name) || self.broken.load(Ordering::SeqCst) {
                return Ok(());
            }
        }
        // Its settings may be older than its secrets until the library is read again.
        if self.broken.load(Ordering::SeqCst) {
            Err(anyhow::anyhow!("the library can't be read; fix it first"))
        } else if self.gateway().is_held(name) {
            // Only a save puts its settings and secrets back in step.
            Err(anyhow::anyhow!(
                "{name} was left disconnected by a save; save it again"
            ))
        } else {
            self.gateway().restart(name)
        }
    }

    /// Starts signing in to server `name` and returns the page to open; see `Request::SignIn`.
    async fn sign_in(self: &Arc<Self>, name: String) -> Result<String> {
        let (url, challenge, tokens) = self.gateway().sign_in_target(&name)?;
        // Finding and registering with the authorization server takes several requests; a slow
        // one shouldn't keep the app waiting without end.
        let sign_in = tokio::time::timeout(
            SIGN_IN_SETUP,
            crate::oauth::start(&url, challenge.as_deref(), tokens),
        )
        .await
        .map_err(|_| anyhow::anyhow!("the server's sign-in didn't answer within a minute"))??;
        let page = sign_in.page.clone();
        self.event(&name, "signing in: waiting for the browser", false);
        let daemon = self.clone();
        let server = name.clone();
        let mut sign_ins = self.sign_ins.lock().unwrap();
        let task = tokio::spawn(async move {
            match sign_in.finish().await {
                Ok(()) => {
                    daemon.event(&server, "signed in", false);
                    if let Err(e) = daemon.reconnect_if_on(&server).await {
                        daemon.event(&server, format!("{e:#}"), true);
                    }
                }
                Err(e) => daemon.event(&server, format!("could not sign in: {e:#}"), true),
            }
            // Unless a newer sign-in has taken its place.
            let mut sign_ins = daemon.sign_ins.lock().unwrap();
            if sign_ins.get(&server).map(|w| w.id()) == Some(tokio::task::id()) {
                sign_ins.remove(&server);
            }
        });
        // The one it replaces can't save its tokens any more.
        if let Some(old) = sign_ins.insert(name, task.abort_handle()) {
            old.abort();
        }
        Ok(page)
    }

    /// Saves a server's settings and secrets as one change; see `Request::SaveServer`.
    async fn save_server(
        self: &Arc<Self>,
        name: String,
        secrets: Vec<(String, Option<String>)>,
        actions: Vec<xolkit_core::Action>,
    ) -> Result<()> {
        anyhow::ensure!(
            xolkit_core::valid_server_name(&name),
            "{name:?} isn't a valid server name"
        );
        let _running = self.executing.lock().await;
        // Nothing changes while the library can't be read: the save couldn't take
        // effect, and the server's connection is left as it is.
        Library::load(&self.env).context("the library can't be read; fix it first")?;
        // No connection may read the new secrets with the old settings, or the reverse. Held
        // on disk before anything changes, so not even a crash mid-save lets it connect; a save
        // that can't record that doesn't start.
        let was_held = self.gateway().is_held(&name);
        let stopped_by_hand = !was_held
            && self
                .gateway()
                .status()
                .iter()
                .any(|s| s.name == name && s.state == crate::protocol::ServerState::Stopped);
        self.gateway().mark(&name)?;
        self.gateway().hold(&name);
        let env = self.env.clone();
        for action in &actions {
            self.event("app", action.describe(&env), false);
        }
        let saved = {
            let name = name.clone();
            tokio::task::spawn_blocking(move || save_server(&env, &name, &secrets, &actions))
                .await
                .unwrap_or_else(|e| Err((anyhow::anyhow!("the change stopped: {e}"), false)))
        };
        // Whether the files now hold settings and secrets that belong together: saved, or put
        // back as they were when they did.
        let consistent = match &saved {
            Ok(()) => true,
            Err((_, restored)) => *restored && !was_held,
        };
        let stuck = format!("{name} stays disconnected: save the server again");
        let read = self.reload();
        self.apply();
        // The settings in use are the ones just read; if the library couldn't be read, they're
        // older ones, which mustn't meet the new secrets.
        // Still held if that can't be written down, or the next daemon would hold it anyway.
        let released = (consistent && read)
            .then(|| self.gateway().release(&name))
            .transpose();
        if let Ok(Some(())) = released {
            // Connected with the settings just read, unless stopped by hand or no longer on
            // for any agent.
            if !stopped_by_hand {
                let _ = self.gateway().restart(&name);
            }
        } else if let Err(e) = &released {
            self.gateway()
                .fail(&name, &format!("{e:#}; save the server again"));
        } else if consistent {
            self.gateway().fail(
                &name,
                "the library can't be read; save the server again once it is fixed",
            );
        } else {
            self.gateway().fail(&name, &stuck);
        }
        if let Err(e) = released {
            let e = e.context(format!("{name} stays disconnected"));
            // A save that failed says so first.
            return Err(match saved {
                Err((error, _)) => error.context(format!("{e:#}")),
                Ok(()) => e,
            });
        }
        match saved {
            Ok(()) if read => Ok(()),
            Ok(()) => Err(anyhow::anyhow!(
                "saved, but the library can't be read; {name} stays disconnected until it is \
                 fixed and the server is saved again"
            )),
            Err((error, _)) if consistent => Err(error),
            Err((error, _)) => Err(error.context(stuck)),
        }
    }

    async fn serve_connection(self: Arc<Self>, stream: Stream, handshake: Handshake) -> Result<()> {
        let (recv, send) = tokio::io::split(stream);
        let mut reader = BufReader::new(recv);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        let hello: Hello = serde_json::from_str(&line).context("a connection said no hello")?;
        match hello {
            Hello::Session {
                agent,
                pid,
                cwd,
                parent,
            } => {
                self.serve_session((reader, send), agent, pid, cwd, parent, handshake)
                    .await
            }
            Hello::Control => {
                // Counted under the sessions' lock, as sessions register, so a daemon that has
                // decided to stop starts no more work for the app either.
                let refused = {
                    let _sessions = self.sessions.lock().unwrap();
                    let stopping = self.stopping.load(Ordering::SeqCst);
                    if !stopping {
                        self.controls.fetch_add(1, Ordering::SeqCst);
                    }
                    stopping
                };
                if !refused {
                    drop(handshake);
                }
                if refused {
                    // Said, so the app knows nothing ran and connects to the next daemon; said in
                    // answer to its request, once that is read, as the app writes its request
                    // first and would find the connection gone.
                    let (mut reader, mut send) = (reader, send);
                    let mut request = String::new();
                    let _ = tokio::time::timeout(
                        REFUSED_READ,
                        (&mut reader)
                            .take(MAX_CONTROL_LINE as u64)
                            .read_line(&mut request),
                    )
                    .await;
                    let line = serde_json::to_string(&Response::Stopping)? + "\n";
                    send.write_all(line.as_bytes()).await?;
                    send.flush().await?;
                    return Ok(());
                }
                let result = self.serve_control(reader, send).await;
                // Active until the last moment the app was there, before it stops counting.
                *self.last_active.lock().unwrap() = Instant::now();
                self.controls.fetch_sub(1, Ordering::SeqCst);
                result
            }
        }
    }

    async fn serve_session(
        self: Arc<Self>,
        (reader, writer): (BufReader<ReadHalf<Stream>>, WriteHalf<Stream>),
        agent: Option<Agent>,
        pid: u32,
        cwd: Option<PathBuf>,
        parent: Option<Parent>,
        handshake: Handshake,
    ) -> Result<()> {
        let agent = agent.unwrap_or(Agent::Other);
        let registered = {
            let mut sessions = self.sessions.lock().unwrap();
            // A daemon on its way out takes no more sessions: the shim starts the next one.
            if self.stopping.load(Ordering::SeqCst) {
                return Ok(());
            }
            // A shim back from idle picks up its session where it left it.
            let idle = sessions
                .values_mut()
                .find(|(s, _)| s.idle && s.pid == pid)
                .map(|(s, _)| s);
            match idle {
                Some(status) => {
                    status.idle = false;
                    status.since = now();
                    status.agent = agent;
                    status.cwd = cwd.clone();
                    status.parent = parent.clone();
                    (status.id, true)
                }
                None => {
                    let id = self.next_session.fetch_add(1, Ordering::SeqCst);
                    let status = SessionStatus {
                        id,
                        agent,
                        client: None,
                        pid,
                        cwd: cwd.clone(),
                        parent,
                        connected: now(),
                        last_call: None,
                        calls: 0,
                        errors: 0,
                        idle: false,
                        since: now(),
                    };
                    sessions.insert(id, (status, None));
                    (id, false)
                }
            }
        };
        let (id, resumed) = registered;
        drop(handshake);
        let source = format!("session {id}");
        if resumed {
            self.event(&source, "back from idle", false);
        } else {
            let place = cwd
                .map(|c| format!(" in {}", self.env.tilde(&c)))
                .unwrap_or_default();
            self.event(&source, format!("{} connected{place}", agent.name()), false);
        }
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
        *self.last_active.lock().unwrap() = Instant::now();
        if let Ok(Err(e)) = &result {
            self.sessions.lock().unwrap().remove(&id);
            self.event(&source, format!("ended: {e:#}"), true);
            return Ok(());
        }
        // The session ended: its agent went away, or its shim let go after a quiet spell and runs
        // on. It is idle at once, so a shim that comes straight back picks it up again. The app's
        // Disconnect has already removed it.
        let kept = {
            let mut sessions = self.sessions.lock().unwrap();
            // A shim that reconnected before this connection was done is a session of its own.
            let back = sessions.values().any(|(s, _)| s.id != id && s.pid == pid);
            match sessions.get_mut(&id) {
                Some((status, abort)) if !back => {
                    status.idle = true;
                    status.since = now();
                    *abort = None;
                    true
                }
                Some(_) => {
                    sessions.remove(&id);
                    false
                }
                None => false,
            }
        };
        if !kept {
            self.event(&source, "disconnected", false);
            return Ok(());
        }
        tokio::time::sleep(SHIM_EXIT).await;
        let runs = {
            let mut sessions = self.sessions.lock().unwrap();
            // Back already, or forgotten: nothing to say.
            if !sessions.get(&id).is_some_and(|(s, _)| s.idle) {
                return Ok(());
            }
            let runs = shim_runs(pid);
            if !runs {
                sessions.remove(&id);
            }
            runs
        };
        self.event(
            &source,
            if runs {
                "idle; its shim reconnects with the agent's next message"
            } else {
                "disconnected"
            },
            false,
        );
        Ok(())
    }

    /// Forgets idle sessions whose shim has exited.
    fn forget_gone(&self) {
        let gone: Vec<u64> = {
            let mut sessions = self.sessions.lock().unwrap();
            let gone: Vec<u64> = sessions
                .values()
                .filter(|(s, _)| s.idle && !shim_runs(s.pid))
                .map(|(s, _)| s.id)
                .collect();
            for id in &gone {
                sessions.remove(id);
            }
            gone
        };
        for id in gone {
            self.event(&format!("session {id}"), "ended while idle", false);
        }
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

    /// Decides to stop when nothing has used the daemon for long enough. It is decided under the
    /// sessions' lock, so no session registers in between and is then cut off.
    fn stop_if_idle(&self) -> bool {
        let minutes = self.library().settings.idle_minutes;
        let sessions = self.sessions.lock().unwrap();
        let idle = sessions.values().all(|(s, _)| s.idle)
            && self.controls.load(Ordering::SeqCst) == 0
            // A browser still on its way back to a sign-in needs someone to come back to.
            && self.sign_ins.lock().unwrap().values().all(|s| s.is_finished())
            && self.last_active.lock().unwrap().elapsed() > Duration::from_secs(minutes * 60);
        if idle {
            self.stopping.store(true, Ordering::SeqCst);
        }
        idle
    }
}

/// Whether the process `pid` still runs, and isn't a zombie. Elsewhere than on Unix the daemon
/// can't tell, and idle sessions drop from the list.
fn shim_runs(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 only checks that the process exists.
        let exists = unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        // A shim that has exited stays a zombie until its agent reaps it.
        #[cfg(target_os = "linux")]
        if exists
            && let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some((_, rest)) = stat.rsplit_once(')')
        {
            return !rest.trim_start().starts_with('Z');
        }
        exists
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Saves a server's secrets, then runs the actions (its settings). If anything fails, the
/// secrets and the server's entry in `servers.toml` are put back as they were; the error says
/// whether that worked (`true`) or left them unsure (`false`).
fn save_server(
    env: &Env,
    name: &str,
    secrets: &[(String, Option<String>)],
    actions: &[xolkit_core::Action],
) -> Result<(), (anyhow::Error, bool)> {
    let mut before = Vec::new();
    for (key, _) in secrets {
        let value = xolkit_core::get_secret(key).map_err(|e| (e, true))?;
        before.push((key.clone(), value));
    }
    let old_config = xolkit_core::read_servers(&env.servers_file())
        .map_err(|e| (e, true))?
        .remove(name);
    let saved = secrets
        .iter()
        .try_for_each(|(key, value)| set_secret(key, value.as_deref()))
        .and_then(|()| execute(env, actions));
    let Err(error) = saved else {
        return Ok(());
    };
    let mut restored = true;
    for (key, value) in &before {
        restored &= set_secret(key, value.as_deref()).is_ok();
    }
    let put_back = xolkit_core::Action::SaveServer {
        name: name.to_owned(),
        config: old_config,
    };
    restored &= execute(env, &[put_back]).is_ok();
    Err((error, restored))
}

/// Opens the log, first moving one past `MAX_LOG` to `daemon.log.old`, so it never grows
/// without end.
fn rotate_log(env: &Env) -> Option<std::fs::File> {
    let log = env.run_dir().join("daemon.log");
    if std::fs::metadata(&log).is_ok_and(|m| m.len() > MAX_LOG) {
        let _ = std::fs::rename(&log, env.run_dir().join("daemon.log.old"));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .ok()
}

/// The modification times of everything the library is read from. An idle shim watches them
/// too, to tell its agent when its lists may have changed.
pub(crate) fn fingerprint(env: &Env) -> Vec<Option<SystemTime>> {
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
    // Every file of every plugin, as its manifest may point anywhere inside it. Each plugin gets
    // its own share of the cap, so one big plugin can't leave the others unwatched, and
    // dependency, build and Git folders are skipped.
    for plugin in folders(&env.plugins_dir()) {
        let mut stack = vec![plugin];
        let mut seen = 0;
        while let Some(entry) = stack.pop() {
            seen += 1;
            if seen > MAX_WATCHED {
                break;
            }
            times.push(mtime(&entry));
            let skip = entry.file_name().is_some_and(|n| {
                [".git", "node_modules", ".venv", "target", "dist"]
                    .iter()
                    .any(|s| n == *s)
            });
            if entry.is_dir() && !skip {
                stack.extend(folders(&entry));
            }
        }
    }
    times
}

/// Runs the daemon until it is told to stop or has been idle long enough.
pub async fn run(env: Env) -> Result<()> {
    let (listener, instance) = ipc::listen(&env)?;
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
            let look = |env: Env| async move {
                // Many stat calls; keep them off the async workers.
                tokio::task::spawn_blocking(move || fingerprint(&env))
                    .await
                    .unwrap_or_default()
            };
            let mut last = look(env.clone()).await;
            loop {
                tokio::time::sleep(WATCH_INTERVAL).await;
                daemon.gateway().reap();
                daemon.forget_gone();
                // Mid-plan, the files are half changed; the plan reloads when it ends.
                let Ok(_idle) = daemon.executing.try_lock() else {
                    continue;
                };
                let now = look(env.clone()).await;
                if now != last {
                    last = now;
                    daemon.reload();
                }
                if daemon.stop_if_idle() {
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
                    daemon.handshaking.fetch_add(1, Ordering::SeqCst);
                    let handshake = Handshake(daemon.clone());
                    tokio::spawn(async move {
                        if let Err(e) = daemon.clone().serve_connection(stream, handshake).await {
                            daemon.event("daemon", format!("connection: {e:#}"), true);
                        }
                    });
                }
                Err(e) => daemon.event("daemon", format!("could not accept: {e}"), true),
            },
        }
    }
    watcher.abort();
    // No more connections while the servers close, which can take a while: a shim coming back
    // from idle meanwhile starts the next daemon instead of losing its request to this one.
    drop(listener);
    ipc::remove_socket(&env, &instance);
    daemon.event("daemon", "stopped", false);
    // Connections already accepted hear that the daemon is stopping before it goes, while the
    // servers close, so the two waits don't add up.
    let handshakes = async {
        let deadline = Instant::now() + REFUSED_READ + Duration::from_secs(1);
        while daemon.handshaking.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::join!(handshakes, daemon.gateway().close_all());
    let _ = std::fs::remove_file(env.run_dir().join("daemon.pid"));
    Ok(())
}
