//! The app's way to `~/.xolkit`: it reads the files itself, and sends every change to the
//! daemon, the only process that writes there.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use gpui_kit::*;
use xolkit_core::{Action, Env, GITHUB_TOKEN, GitHubAccess, Settings, get_secret, gh_token};
use xolkit_daemon::{Client, Request};

pub struct Store {
    /// The folders; the `PATH` for agents' tools is the daemon's business.
    pub env: Env,
    /// This program: what agents are registered to start, and what starts the daemon.
    pub exe: PathBuf,
    client: Arc<Mutex<Client>>,
}

impl Global for Store {}

pub fn init(cx: &mut App) {
    let env = Env::detect_dirs().unwrap_or_else(|_| Env::at(Path::new(".")));
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("xolkit"));
    let client = Arc::new(Mutex::new(Client::new(env.clone(), &exe)));
    #[cfg(windows)]
    {
        let exe = exe.clone();
        std::thread::spawn(move || register_url_handler(&exe));
    }
    cx.set_global(Store { env, exe, client });
}

pub fn store(cx: &App) -> &Store {
    cx.global::<Store>()
}

/// The daemon client, for a background task.
pub fn client(cx: &App) -> Arc<Mutex<Client>> {
    store(cx).client.clone()
}

/// The settings as saved; defaults when the file is missing or unreadable.
pub fn load(cx: &App) -> Settings {
    Settings::load(&store(cx).env.settings_file()).unwrap_or_default()
}

/// Sends `request` to the daemon and waits for its answer. Blocks: call it off the UI thread.
pub fn call(client: &Mutex<Client>, request: Request) -> Result<()> {
    client.lock().unwrap().call(&request)
}

pub fn execute(client: &Mutex<Client>, actions: Vec<Action>) -> Result<()> {
    client.lock().unwrap().execute(actions)
}

/// Saves the settings through the daemon in the background, reporting a failure on stderr. For
/// choices like the theme, which take effect in the app at once whatever happens to the file.
pub fn save_in_background(settings: Settings, cx: &App) {
    let client = client(cx);
    cx.background_spawn(async move {
        let saved = execute(
            &client,
            vec![Action::SaveSettings {
                settings: Box::new(settings),
            }],
        );
        if let Err(e) = saved {
            eprintln!("xolkit: could not save the settings: {e:#}");
        }
    })
    .detach();
}

/// The folders and the `PATH` the user's login shell sets, worked out once: an app started from
/// a desktop launcher has a short `PATH`, without `gh` in `~/.local/bin`, say.
fn full_env(env: &Env) -> Env {
    static FULL: std::sync::OnceLock<Env> = std::sync::OnceLock::new();
    FULL.get_or_init(|| match Env::detect() {
        Ok(mut full) => {
            full.xolkit_dir = env.xolkit_dir.clone();
            full
        }
        Err(_) => env.clone(),
    })
    .clone()
}

/// The GitHub token to send for `access`, if any. Runs `gh`, so call it off the UI thread.
pub fn github_token(access: GitHubAccess, env: &Env) -> Result<Option<String>> {
    match access {
        GitHubAccess::Gh => gh_token(&full_env(env)).map(Some),
        GitHubAccess::Token => get_secret(GITHUB_TOKEN)?
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("no token is saved")),
        GitHubAccess::None => Ok(None),
    }
}

/// Makes `xolkit://` links open this program, for the current user. Linux does this with the
/// desktop entry instead.
#[cfg(windows)]
fn register_url_handler(exe: &Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let key = r"HKCU\Software\Classes\xolkit";
    let command = format!("\"{}\" \"%1\"", exe.display());
    let entries: [&[&str]; 3] = [
        &["add", key, "/ve", "/d", "URL:xolkit profile", "/f"],
        &["add", key, "/v", "URL Protocol", "/d", "", "/f"],
        &[
            "add",
            &format!(r"{key}\shell\open\command"),
            "/ve",
            "/d",
            &command,
            "/f",
        ],
    ];
    for args in entries {
        let _ = std::process::Command::new("reg")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output();
    }
}
