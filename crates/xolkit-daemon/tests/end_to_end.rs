//! A daemon in a throwaway home, an agent session through its socket, and the app's control
//! connection: skills, prompts, a passed-on MCP server, and a profile change reaching the
//! session.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rmcp::model::{CallToolRequestParams, GetPromptRequestParams};
use rmcp::service::NotificationContext;
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use tokio::io::AsyncWriteExt;
use xolkit_core::{Action, Agent, Env, Kind, ServerConfig, Settings};
use xolkit_daemon::{Client, Hello, ServerState};

#[derive(Clone, Default)]
struct Agentish {
    tools_changed: Arc<AtomicUsize>,
}

impl ClientHandler for Agentish {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.tools_changed.fetch_add(1, Ordering::SeqCst);
    }
}

fn write_skill(env: &Env, name: &str) {
    let dir = env.skills_dir().join(name);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Ships {name} to production.\n---\n\nRun scripts/go.sh.\n"),
    )
    .unwrap();
    std::fs::write(dir.join("scripts/go.sh"), "echo go").unwrap();
}

async fn wait_for(what: &str, mut done: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serves_an_agent_session() {
    let home = tempfile::tempdir().unwrap();
    let env = Env::at(home.path());
    write_skill(&env, "deploy");
    write_skill(&env, "later");
    let mut settings = Settings::default();
    let profile = settings.active_mut();
    profile.agents = vec![Agent::Claude];
    profile.set(Kind::Skill, "deploy", Agent::Claude, true);
    profile.set(Kind::Server, "echo", Agent::Claude, true);
    std::fs::create_dir_all(&env.xolkit_dir).unwrap();
    settings.save(&env.settings_file()).unwrap();
    xolkit_core::execute(
        &env,
        &[Action::SaveServer {
            name: "echo".into(),
            config: Some(ServerConfig {
                command: Some(env!("CARGO_BIN_EXE_echo-server").into()),
                ..ServerConfig::default()
            }),
        }],
    )
    .unwrap();

    let daemon = {
        let env = env.clone();
        std::thread::spawn(move || xolkit_daemon::run_daemon(env))
    };
    wait_for("the daemon", async || {
        xolkit_daemon::connect(&env).await.is_ok()
    })
    .await;

    // An agent session, the way the shim opens one.
    let mut stream = xolkit_daemon::connect(&env).await.unwrap();
    let hello = Hello::Session {
        agent: Some(Agent::Claude),
        // A process that runs on after the session ends, as a shim that goes idle does.
        pid: std::process::id(),
        cwd: Some(home.path().to_path_buf()),
        parent: None,
    };
    stream
        .write_all(format!("{}\n", serde_json::to_string(&hello).unwrap()).as_bytes())
        .await
        .unwrap();
    let agent = Agentish::default();
    let session = agent.clone().serve(tokio::io::split(stream)).await.unwrap();

    let info = session.peer_info().unwrap();
    let instructions = info.instructions.clone().unwrap_or_default();
    assert!(
        instructions.contains("- deploy: Ships deploy to production."),
        "{instructions}"
    );
    assert!(!instructions.contains("later"), "{instructions}");

    let loaded = session
        .call_tool(
            CallToolRequestParams::new("load_skill").with_arguments(
                serde_json::json!({"name": "deploy"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await
        .unwrap();
    let text = serde_json::to_string(&loaded.content).unwrap();
    assert!(text.contains("Run scripts/go.sh."), "{text}");
    assert!(text.contains("scripts/go.sh"), "{text}");

    let prompts = session.list_all_prompts().await.unwrap();
    assert_eq!(
        prompts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        ["deploy"]
    );
    let prompt = session
        .get_prompt(GetPromptRequestParams::new("deploy"))
        .await
        .unwrap();
    assert!(
        serde_json::to_string(&prompt.messages)
            .unwrap()
            .contains("Run scripts/go.sh.")
    );

    // The echo server's tool, passed on once the daemon has connected to it.
    let tools = loop {
        let tools = session.list_all_tools().await.unwrap();
        if tools.iter().any(|t| t.name == "echo__echo") {
            break tools;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(tools.iter().any(|t| t.name == "find_skill"));
    let echoed = session
        .call_tool(
            CallToolRequestParams::new("echo__echo").with_arguments(
                serde_json::json!({"text": "hi"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await
        .unwrap();
    assert!(
        serde_json::to_string(&echoed.content)
            .unwrap()
            .contains("echo: hi")
    );

    // The app sees the session and the server.
    let mut app = Client::new(env.clone(), Path::new("/no/xolkit"));
    let status = tokio::task::spawn_blocking(move || {
        let status = app.status().unwrap();
        (app, status)
    });
    let (mut app, status) = status.await.unwrap();
    assert_eq!(status.sessions.len(), 1);
    assert_eq!(status.sessions[0].agent, Agent::Claude);
    assert!(status.sessions[0].calls >= 3);
    assert_eq!(status.servers[0].state, ServerState::Running);
    assert_eq!(status.servers[0].tools, 1);

    // The app turns the other skill on: the session hears its lists changed.
    let before = agent.tools_changed.load(Ordering::SeqCst);
    settings
        .active_mut()
        .set(Kind::Skill, "later", Agent::Claude, true);
    let app = tokio::task::spawn_blocking(move || {
        app.execute(vec![Action::SaveSettings {
            settings: Box::new(settings),
        }])
        .unwrap();
        app
    })
    .await
    .unwrap();
    wait_for("the list change", async || {
        agent.tools_changed.load(Ordering::SeqCst) > before
    })
    .await;
    let found = session
        .call_tool(
            CallToolRequestParams::new("find_skill").with_arguments(
                serde_json::json!({"query": "later"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await
        .unwrap();
    assert!(
        serde_json::to_string(&found.content)
            .unwrap()
            .contains("later (skill)")
    );

    // Adding a server through the one-step save connects it.
    let echo = ServerConfig {
        command: Some(env!("CARGO_BIN_EXE_echo-server").into()),
        ..ServerConfig::default()
    };
    let mut with_second = Settings::load(&env.settings_file()).unwrap();
    with_second
        .active_mut()
        .set(Kind::Server, "echo2", Agent::Claude, true);
    let app = tokio::task::spawn_blocking(move || {
        let mut app = app;
        app.call(&xolkit_daemon::Request::SaveServer {
            name: "echo2".into(),
            secrets: Vec::new(),
            actions: vec![
                Action::SaveServer {
                    name: "echo2".into(),
                    config: Some(echo),
                },
                Action::SaveSettings {
                    settings: Box::new(with_second),
                },
            ],
        })
        .unwrap();
        app
    })
    .await
    .unwrap();
    let app = std::sync::Arc::new(std::sync::Mutex::new(app));
    wait_for("the added server", async || {
        let app = app.clone();
        tokio::task::spawn_blocking(move || {
            let status = app.lock().unwrap().status().unwrap();
            status
                .servers
                .iter()
                .any(|s| s.name == "echo2" && s.state == ServerState::Running)
        })
        .await
        .unwrap()
    })
    .await;
    let app = std::sync::Arc::try_unwrap(app)
        .ok()
        .unwrap()
        .into_inner()
        .unwrap();

    // The session ends while its "shim" runs on: the daemon keeps it, idle, and picks it up
    // again when the same shim connects.
    let id = status.sessions[0].id;
    session.cancel().await.unwrap();
    let app = std::sync::Arc::new(std::sync::Mutex::new(app));
    let idle = async |want: bool| {
        let app = app.clone();
        tokio::task::spawn_blocking(move || {
            let status = app.lock().unwrap().status().unwrap();
            status.sessions.len() == 1
                && status.sessions[0].id == id
                && status.sessions[0].idle == want
        })
        .await
        .unwrap()
    };
    wait_for("the idle session", async || idle(true).await).await;
    let mut stream = xolkit_daemon::connect(&env).await.unwrap();
    stream
        .write_all(format!("{}\n", serde_json::to_string(&hello).unwrap()).as_bytes())
        .await
        .unwrap();
    let session = Agentish::default()
        .serve(tokio::io::split(stream))
        .await
        .unwrap();
    wait_for("the session back", async || idle(false).await).await;

    // The shim connects again before its old connection is done: the old one leaves no idle
    // session of its own behind.
    let open = async || {
        let mut stream = xolkit_daemon::connect(&env).await.unwrap();
        stream
            .write_all(format!("{}\n", serde_json::to_string(&hello).unwrap()).as_bytes())
            .await
            .unwrap();
        Agentish::default()
            .serve(tokio::io::split(stream))
            .await
            .unwrap()
    };
    let newer = open().await;
    session.cancel().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let sessions = {
        let app = app.clone();
        tokio::task::spawn_blocking(move || app.lock().unwrap().status().unwrap().sessions)
            .await
            .unwrap()
    };
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    assert!(sessions[0].id != id && !sessions[0].idle, "{sessions:?}");
    newer.cancel().await.unwrap();
    let app = std::sync::Arc::try_unwrap(app)
        .ok()
        .unwrap()
        .into_inner()
        .unwrap();
    tokio::task::spawn_blocking(move || {
        let mut app = app;
        app.call(&xolkit_daemon::Request::Shutdown).unwrap();
    })
    .await
    .unwrap();
    daemon.join().unwrap().unwrap();
    assert!(!env.run_dir().join("daemon.pid").exists());
}
