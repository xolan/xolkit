use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::Provider;

/// Windows `CREATE_NO_WINDOW`: a console program started from a GUI app runs without opening a
/// console window of its own.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Where xolkit and the providers keep their files, and how to run the providers' tools.
#[derive(Clone, Debug)]
pub struct Env {
    pub home: PathBuf,
    /// `~/.xolkit`, or `$XOLKIT_HOME`: everything xolkit keeps.
    pub xolkit_dir: PathBuf,
    /// `~/.claude`, or `$CLAUDE_CONFIG_DIR`.
    pub claude_dir: PathBuf,
    /// `~/.codex`, or `$CODEX_HOME`.
    pub codex_dir: PathBuf,
    /// `PATH` for provider tools: this process's `PATH`, then on Unix any extra entries from the
    /// user's login shell, because an app started from a desktop launcher gets a short `PATH`.
    pub path: Option<OsString>,
}

impl Env {
    pub fn detect() -> Result<Env> {
        let mut env = Env::detect_dirs()?;
        env.path = merge_paths(std::env::var_os("PATH"), login_shell_path());
        Ok(env)
    }

    /// The folders only, without asking the login shell for its `PATH`, which can take a few
    /// seconds: enough to read the library and reach the daemon, not to run an agent's tool.
    pub fn detect_dirs() -> Result<Env> {
        let home = std::env::home_dir().context("no home directory")?;
        let dir_or = |var: &str, default: &str| {
            std::env::var_os(var)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(default))
        };
        Ok(Env {
            xolkit_dir: dir_or("XOLKIT_HOME", ".xolkit"),
            claude_dir: dir_or("CLAUDE_CONFIG_DIR", ".claude"),
            codex_dir: dir_or("CODEX_HOME", ".codex"),
            path: None,
            home,
        })
    }

    /// An environment rooted at `home`, for tests.
    pub fn at(home: &Path) -> Env {
        Env {
            home: home.to_path_buf(),
            xolkit_dir: home.join(".xolkit"),
            claude_dir: home.join(".claude"),
            codex_dir: home.join(".codex"),
            path: None,
        }
    }

    /// Installed skills, one folder each.
    pub fn skills_dir(&self) -> PathBuf {
        self.xolkit_dir.join("skills")
    }

    /// Installed plugins, one folder each, named by plugin id.
    pub fn plugins_dir(&self) -> PathBuf {
        self.xolkit_dir.join("plugins")
    }

    /// The local marketplace the agents install native plugins from.
    pub fn marketplace_dir(&self) -> PathBuf {
        self.xolkit_dir.join("marketplace")
    }

    pub fn settings_file(&self) -> PathBuf {
        self.xolkit_dir.join("settings.json")
    }

    /// Where skills and plugins came from, and the commits they are pinned to.
    pub fn lock_file(&self) -> PathBuf {
        self.xolkit_dir.join("lock.json")
    }

    /// The MCP servers xolkit connects to.
    pub fn servers_file(&self) -> PathBuf {
        self.xolkit_dir.join("servers.toml")
    }

    /// The daemon's socket, pid file and log.
    pub fn run_dir(&self) -> PathBuf {
        self.xolkit_dir.join("run")
    }

    /// Downloads wait here until they are installed: inside `~/.xolkit`, so installing is a
    /// rename, but outside `skills/`, so no agent sees a skill that is only being previewed.
    pub fn staging_dir(&self) -> PathBuf {
        self.xolkit_dir.join("staging")
    }

    /// Extra Base16 themes.
    pub fn themes_dir(&self) -> PathBuf {
        self.xolkit_dir.join("themes")
    }

    /// Claude Code's own config, where `claude mcp add --scope user` writes: `~/.claude.json`,
    /// or `.claude.json` inside `$CLAUDE_CONFIG_DIR` when that moves Claude's directory.
    pub fn claude_json(&self) -> PathBuf {
        if self.claude_dir == self.home.join(".claude") {
            self.home.join(".claude.json")
        } else {
            self.claude_dir.join(".claude.json")
        }
    }

    pub fn claude_skills(&self) -> PathBuf {
        self.claude_dir.join("skills")
    }

    pub fn codex_skills(&self) -> PathBuf {
        self.codex_dir.join("skills")
    }

    pub fn codex_config(&self) -> PathBuf {
        self.codex_dir.join("config.toml")
    }

    pub fn codex_plugin_cache(&self) -> PathBuf {
        self.codex_dir.join("plugins").join("cache")
    }

    /// `path` with the home directory shown as `~`: `~/…`, or `~\…` on Windows.
    pub fn tilde(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
            Err(_) => path.display().to_string(),
        }
    }

    /// Runs a provider tool and returns its stdout.
    pub fn run(&self, program: &str, args: &[String]) -> Result<String> {
        let mut command = Command::new(self.find(program));
        // Run from home: a project's tool config (e.g. a mise.toml pinning another codex) must
        // not leak into global changes.
        command
            .args(args)
            .current_dir(&self.home)
            .stdin(Stdio::null());
        if let Some(path) = &self.path {
            command.env("PATH", path);
        }
        // Provider tools are console programs; each would flash a console window over the app.
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut command, CREATE_NO_WINDOW);
        let output = output_within(command, RUN_TIMEOUT)
            .with_context(|| format!("could not run `{program}`"))?;
        if !output.status.success() {
            bail!(
                "`{program} {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// The file to start for `program`. On Windows, `Command` only looks for `<program>.exe`,
    /// but npm installs `claude.cmd` and `codex.cmd`, so look on `PATH` the way a shell does.
    pub fn find(&self, program: &str) -> PathBuf {
        if cfg!(windows)
            && let Some(found) = self.tool_path(program)
        {
            return found;
        }
        PathBuf::from(program)
    }

    /// Whether `program` is missing from the user's whole `PATH` as it is now: from the login
    /// shell, or on Windows from the registry, as a newly started program would get it. `false`
    /// when that can't be told.
    pub fn tool_gone(&self, program: &str) -> bool {
        let Some(login) = fresh_path() else {
            return false;
        };
        let fresh = Env {
            path: merge_paths(std::env::var_os("PATH"), Some(login)),
            ..self.clone()
        };
        !fresh.has_tool(program)
    }

    /// Whether `program` is on `PATH`.
    pub fn has_tool(&self, program: &str) -> bool {
        self.tool_path(program).is_some()
    }

    /// Where `program` is on `PATH`, if it is.
    fn tool_path(&self, program: &str) -> Option<PathBuf> {
        let path = self.path.clone().or_else(|| std::env::var_os("PATH"))?;
        #[cfg(windows)]
        let found = find_on_path(program, &path);
        #[cfg(not(windows))]
        let found = std::env::split_paths(&path)
            .map(|dir| dir.join(program))
            .find(|file| file.is_file());
        found
    }

    /// Where the provider shows it is installed: its directory, or else its tool on `PATH`.
    pub fn locate(&self, provider: Provider) -> Option<PathBuf> {
        let dir = match provider {
            Provider::Claude => &self.claude_dir,
            Provider::Codex => &self.codex_dir,
        };
        if dir.is_dir() {
            return Some(dir.clone());
        }
        self.tool_path(provider.cli())
    }
}

/// How long an agent's tool may run before it counts as hung and is stopped.
const RUN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// How long to wait for a pipe to close once the process exited: a helper it left running can
/// hold the pipe open for good.
const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// A pipe read on its own thread into a buffer, so a full pipe never stalls the process, and
/// whatever arrived can be taken without waiting for the pipe to close.
struct Drain {
    buffer: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    done: std::sync::mpsc::Receiver<()>,
}

/// Most output kept from one pipe; past it, the rest is read and dropped.
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

impl Drain {
    fn new(pipe: Option<impl std::io::Read + Send + 'static>) -> Drain {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (sender, done) = std::sync::mpsc::channel();
        // Once the buffer is taken, the thread holds the only other handle; a helper that keeps
        // writing then has its output dropped, not kept.
        let filling = std::sync::Arc::downgrade(&buffer);
        std::thread::spawn(move || {
            if let Some(mut pipe) = pipe {
                let mut chunk = [0; 8192];
                while let Ok(n @ 1..) = pipe.read(&mut chunk) {
                    if let Some(buffer) = filling.upgrade() {
                        let mut buffer = buffer.lock().unwrap();
                        let room = MAX_OUTPUT.saturating_sub(buffer.len());
                        buffer.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
            let _ = sender.send(());
        });
        Drain { buffer, done }
    }

    /// What was read, once the pipe closes or `grace` passes.
    fn take(self, grace: std::time::Duration) -> Vec<u8> {
        let _ = self.done.recv_timeout(grace);
        std::mem::take(&mut *self.buffer.lock().unwrap())
    }
}

/// Runs `command` to the end and collects its output, killing it after `timeout`.
fn output_within(
    mut command: Command,
    timeout: std::time::Duration,
) -> Result<std::process::Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = Drain::new(child.stdout.take());
    let stderr = Drain::new(child.stderr.take());
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "it took over {} minutes, so it was stopped",
                timeout.as_secs() / 60
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    Ok(std::process::Output {
        status,
        stdout: stdout.take(DRAIN_GRACE),
        stderr: stderr.take(DRAIN_GRACE),
    })
}

/// The first `<dir>\<program><ext>` on `path`, trying the extensions in `PATHEXT` that
/// `CreateProcess` can start: programs and batch files, not scripts such as `.ps1` or `.js`.
#[cfg(windows)]
fn find_on_path(program: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let extensions: Vec<String> = pathext
        .split(';')
        .map(str::to_ascii_lowercase)
        .filter(|ext| [".com", ".exe", ".bat", ".cmd"].contains(&ext.as_str()))
        .collect();
    std::env::split_paths(path).find_map(|dir| {
        extensions
            .iter()
            .map(|ext| dir.join(format!("{program}{ext}")))
            .find(|file| file.is_file())
    })
}

/// `first`, then the entries of `extra` that `first` lacks.
fn merge_paths(first: Option<OsString>, extra: Option<OsString>) -> Option<OsString> {
    let mut dirs: Vec<PathBuf> = first.iter().flat_map(std::env::split_paths).collect();
    for dir in extra.iter().flat_map(std::env::split_paths) {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    std::env::join_paths(dirs).ok().filter(|p| !p.is_empty())
}

/// `PATH` as the user's login shell sets it, or `None` if the shell fails or takes over 5 s.
#[cfg(unix)]
fn login_shell_path() -> Option<OsString> {
    use std::time::{Duration, Instant};
    let shell = std::env::var_os("SHELL")?;
    let mut command = Command::new(shell);
    command
        .args(["-l", "-i", "-c", "env"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // An interactive shell in a background process group of a terminal stops itself, and the
    // whole group with it, when it touches the terminal; its own session has no terminal.
    // SAFETY: setsid is async-signal-safe, which is all pre_exec requires.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut command, || {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().ok()?;
    let stdout = Drain::new(child.stdout.take());
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().ok()?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = stdout.take(DRAIN_GRACE);
    String::from_utf8_lossy(&output)
        .lines()
        .find_map(|line| line.strip_prefix("PATH="))
        .map(OsString::from)
}

/// Windows apps get the user's full `PATH` from the registry however they are started.
#[cfg(not(unix))]
fn login_shell_path() -> Option<OsString> {
    None
}

/// The `PATH` a program started now would get.
#[cfg(unix)]
fn fresh_path() -> Option<OsString> {
    login_shell_path()
}

/// The `PATH` a program started now would get: the system's and the user's from the registry,
/// with `%VARIABLES%` expanded. A running program's own `PATH` misses changes made since.
#[cfg(windows)]
fn fresh_path() -> Option<OsString> {
    use std::os::windows::process::CommandExt;
    let read = |key: &str| -> Option<String> {
        let output = Command::new("reg")
            .args(["query", key, "/v", "Path"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        // `    Path    REG_EXPAND_SZ    C:\x;%USERPROFILE%\y`
        text.lines().find_map(|line| {
            let rest = line.trim_start().strip_prefix("Path")?.trim_start();
            let (_kind, value) = rest.split_once(char::is_whitespace)?;
            Some(value.trim().to_owned())
        })
    };
    let system = read(r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment");
    let user = read(r"HKCU\Environment");
    if system.is_none() && user.is_none() {
        return None;
    }
    let joined = [system, user]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(";");
    Some(OsString::from(expand_variables(&joined)))
}

/// `text` with each `%NAME%` replaced by that environment variable, or kept when it isn't set.
#[cfg(any(windows, test))]
fn expand_variables(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(value) if !name.is_empty() => out.push_str(&value),
                    _ => out.push_str(&rest[start..start + end + 2]),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_use_the_platform_separator() {
        let home = if cfg!(windows) { r"C:\h" } else { "/h" };
        let env = Env::at(Path::new(home));
        let shown = env.tilde(&env.skills_dir().join("s"));
        if cfg!(windows) {
            assert_eq!(shown, r"~\.xolkit\skills\s");
            assert_eq!(env.lock_file(), Path::new(r"C:\h\.xolkit\lock.json"));
            assert_eq!(
                env.codex_plugin_cache().to_str(),
                Some(r"C:\h\.codex\plugins\cache")
            );
        } else {
            assert_eq!(shown, "~/.xolkit/skills/s");
        }
        assert_eq!(env.tilde(Path::new("../x")), "../x");
    }

    #[test]
    fn expands_variables_in_registry_paths() {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap();
        let key = if std::env::var("HOME").is_ok() {
            "HOME"
        } else {
            "USERPROFILE"
        };
        assert_eq!(
            expand_variables(&format!("%{key}%\\bin;%NOT_SET_XK%;50%")),
            format!("{home}\\bin;%NOT_SET_XK%;50%")
        );
    }

    #[test]
    fn merge_paths_keeps_own_order_first() {
        let paths = |dirs: &[&str]| std::env::join_paths(dirs).ok();
        let merged = merge_paths(paths(&["/a", "/b"]), paths(&["/c", "/a", "/d"]));
        assert_eq!(merged, paths(&["/a", "/b", "/c", "/d"]));
        assert_eq!(merge_paths(None, paths(&["/c"])), paths(&["/c"]));
        assert_eq!(merge_paths(None, None), None);
    }

    #[test]
    fn locates_providers_by_directory_or_tool() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::create_dir(&bin).unwrap();
        let mut env = Env::at(dir.path());
        env.path = Some(bin.clone().into());
        assert_eq!(
            env.locate(Provider::Claude),
            Some(dir.path().join(".claude"))
        );
        assert_eq!(env.locate(Provider::Codex), None);
        let tool = bin.join(if cfg!(windows) { "codex.cmd" } else { "codex" });
        std::fs::write(&tool, "").unwrap();
        assert_eq!(env.locate(Provider::Codex), Some(tool));
    }

    #[cfg(windows)]
    #[test]
    fn runs_batch_file_tools_found_on_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tool.cmd"), "@echo ran %*\r\n").unwrap();
        std::fs::write(dir.path().join("script.ps1"), "").unwrap();
        let mut env = Env::at(dir.path());
        env.path = Some(dir.path().into());
        let args = ["plugin".to_owned(), "name@market".to_owned()];
        assert_eq!(
            env.run("tool", &args).unwrap().trim(),
            "ran plugin name@market"
        );
        // PowerShell scripts are not programs CreateProcess can start.
        assert!(env.run("script", &[]).is_err());
    }
}
