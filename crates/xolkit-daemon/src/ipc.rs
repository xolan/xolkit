//! The daemon's socket: a Unix socket in `~/.xolkit/run` that only the user can open, or on
//! Windows a named pipe only the user can open. Nothing listens on the network.

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use interprocess::local_socket::tokio::{Listener, Stream as AsyncStream};
use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::traits::tokio::Stream as _;
use interprocess::local_socket::{ListenerOptions, Name, Stream};
use xolkit_core::Env;

/// How long a shim or the app waits for a daemon it started to listen.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// The socket's name for the xolkit folder `env` uses, so a test home gets a daemon of its own.
#[cfg(unix)]
fn name(env: &Env) -> io::Result<Name<'static>> {
    use interprocess::local_socket::{GenericFilePath, ToFsName};
    env.run_dir()
        .join("daemon.sock")
        .to_fs_name::<GenericFilePath>()
}

/// Named pipes live in one namespace for every user, so the daemon picks a random name and
/// keeps it in `~/.xolkit/run/pipe`, which only the user can read: no one else can guess it,
/// or set up a pipe of that name first.
#[cfg(windows)]
fn name(env: &Env) -> io::Result<Name<'static>> {
    use interprocess::local_socket::{GenericNamespaced, ToNsName};
    let pipe = std::fs::read_to_string(env.run_dir().join("pipe"))?;
    pipe.trim().to_owned().to_ns_name::<GenericNamespaced>()
}

#[cfg(windows)]
fn new_pipe_name() -> String {
    use std::hash::{BuildHasher, Hasher};
    // Each RandomState is seeded from the system's random source.
    let part = || {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        hasher.finish()
    };
    format!("xolkit-{:016x}{:016x}", part(), part())
}

/// Held for as long as the daemon runs: only one daemon a user's xolkit folder.
pub(crate) struct Instance {
    _lock: std::fs::File,
}

/// Takes the daemon's lock and listens on the socket. Fails when another daemon runs.
pub(crate) fn listen(env: &Env) -> Result<(Listener, Instance)> {
    let run = env.run_dir();
    std::fs::create_dir_all(&run)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700))?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(run.join("daemon.lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!("another xolkit daemon is running"),
        Err(std::fs::TryLockError::Error(e)) => return Err(e).context("could not lock the daemon"),
    }
    #[cfg(windows)]
    std::fs::write(run.join("pipe"), new_pipe_name())?;
    let options = ListenerOptions::new().name(name(env)?);
    #[cfg(unix)]
    let options = {
        use interprocess::os::unix::local_socket::ListenerOptionsExt;
        // A socket left by a daemon that crashed is replaced; the lock rules out a live one.
        options.try_overwrite(true).mode(0o600)
    };
    #[cfg(windows)]
    let options = {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;
        // Full access for the owner and the system, nobody else; no inherited entries.
        let sddl = widestring::U16CString::from_str("D:P(A;;GA;;;OW)(A;;GA;;;SY)")
            .expect("the SDDL has no nul");
        options.security_descriptor(SecurityDescriptor::deserialize(&sddl)?)
    };
    let listener = options
        .create_tokio()
        .context("could not listen on the daemon's socket")?;
    Ok((listener, Instance { _lock: lock }))
}

/// Removes the socket file once the daemon is done with it, while it still holds the lock.
pub(crate) fn remove_socket(env: &Env, _instance: &Instance) {
    let _ =
        std::fs::remove_file(
            env.run_dir()
                .join(if cfg!(unix) { "daemon.sock" } else { "pipe" }),
        );
}

pub(crate) async fn connect_async(env: &Env) -> io::Result<AsyncStream> {
    AsyncStream::connect(name(env)?).await
}

/// Connects to the daemon, starting it first when it isn't running.
pub(crate) async fn connect_or_start(env: &Env, exe: &Path) -> Result<AsyncStream> {
    if let Ok(stream) = connect_async(env).await {
        return Ok(stream);
    }
    start(exe)?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        match connect_async(env).await {
            Ok(stream) => return Ok(stream),
            Err(e) if Instant::now() > deadline => {
                return Err(e).context("the xolkit daemon didn't start");
            }
            Err(_) => {}
        }
    }
}

/// Connects to the daemon without an async runtime, for the app, starting it when needed.
pub(crate) fn connect_or_start_blocking(env: &Env, exe: &Path) -> Result<Stream> {
    if let Ok(stream) = Stream::connect(name(env)?) {
        return Ok(stream);
    }
    start(exe)?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        std::thread::sleep(Duration::from_millis(100));
        match Stream::connect(name(env)?) {
            Ok(stream) => return Ok(stream),
            Err(e) if Instant::now() > deadline => {
                return Err(e).context("the xolkit daemon didn't start");
            }
            Err(_) => {}
        }
    }
}

/// Starts `exe daemon` on its own: no terminal, and it outlives whoever started it.
fn start(exe: &Path) -> Result<()> {
    let mut command = Command::new(exe);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    #[cfg(windows)]
    {
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::os::windows::process::CommandExt::creation_flags(
            &mut command,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
        );
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("could not start {}", exe.display()))?;
    // Collects its exit status when it ends, so it leaves no zombie behind.
    std::thread::spawn(move || child.wait());
    Ok(())
}
