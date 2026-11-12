//! Secrets in the OS keyring: the Secret Service on Linux, the Credential Manager on Windows.
//! Nothing secret is written to `~/.xolkit`, so the folder can be backed up or kept in a
//! dotfiles repository.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use keyring_core::{Entry, Error};

const SERVICE: &str = "xolkit";

/// The keyring entry for the GitHub token typed under Settings.
pub const GITHUB_TOKEN: &str = "github-token";

/// Whether the OS keyring has been reached. While it has not, uses try again, so a keyring that
/// starts or comes back after xolkit did is found without restarting xolkit.
enum Keyring {
    Unreached,
    /// It took long to fail, for the reason `why`, and is not tried again before `until`.
    Hung {
        until: Instant,
        why: String,
    },
    Reached,
}

/// Held for the whole of each use, so uses take turns across the process and one that waited
/// on a keyring that hung sees that it did. A use that waits on the keyring, for the user to
/// unlock it or for a provider that never answers, keeps every other use waiting with it.
static KEYRING: Mutex<Keyring> = Mutex::new(Keyring::Unreached);

/// How long a use of the keyring may take to fail before the keyring counts as hung.
const SLOW: Duration = Duration::from_secs(1);

impl Keyring {
    /// The keyring after a use begun at `started` failed for the reason `why`. The next use
    /// reaches for it anew, as the connection may be lost: a Secret Service provider that was
    /// restarted, say. That is at once if the failure came quickly, so a caller can still undo
    /// what it half did. A keyring that is slow to fail can take many seconds over it and
    /// callers ask for one secret after another, so after a slow failure it is left alone for
    /// as long as the failure took. None of this helps with one that never answers at all.
    fn after_failure(why: String, started: Instant) -> Self {
        let took = started.elapsed();
        if took < SLOW {
            Keyring::Unreached
        } else {
            Keyring::Hung {
                until: Instant::now() + took,
                why,
            }
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
const UNREACHABLE: &str = "could not reach the OS keyring: is a Secret Service provider, such as \
                           GNOME Keyring, KWallet or KeePassXC, running?";
#[cfg(not(all(unix, not(target_os = "macos"))))]
const UNREACHABLE: &str = "could not reach the OS keyring";

fn store() -> keyring_core::Result<Arc<keyring_core::CredentialStore>> {
    #[cfg(windows)]
    let store = windows_native_keyring_store::Store::new()?;
    #[cfg(target_os = "macos")]
    let store = apple_native_keyring_store::keychain::Store::new()?;
    #[cfg(all(unix, not(target_os = "macos")))]
    let store = zbus_secret_service_keyring_store::Store::new()?;
    Ok(store)
}

/// What `op` gives for the keyring entry of `key`, or `None` if there is no such entry. An
/// error says `what` could not be done.
fn with_entry<T>(
    key: &str,
    what: &'static str,
    op: impl FnOnce(&Entry) -> keyring_core::Result<T>,
) -> Result<Option<T>> {
    let mut keyring = KEYRING.lock().unwrap_or_else(PoisonError::into_inner);
    if let Keyring::Hung { until, why } = &*keyring
        && Instant::now() < *until
    {
        return Err(anyhow!("it hung a moment ago: {why}")).context(what);
    }
    let started = Instant::now();
    if !matches!(*keyring, Keyring::Reached) {
        match store() {
            Ok(store) => {
                keyring_core::set_default_store(store);
                *keyring = Keyring::Reached;
            }
            Err(e) => {
                let why = format!("{UNREACHABLE}: {e}");
                *keyring = Keyring::after_failure(why.clone(), started);
                return Err(anyhow!(why)).context(what);
            }
        }
    }
    match Entry::new(SERVICE, key).and_then(|entry| op(&entry)) {
        Ok(value) => Ok(Some(value)),
        Err(Error::NoEntry) => Ok(None),
        Err(e) => {
            // A keyring that is locked, or that refuses one entry, is still connected.
            if matches!(e, Error::PlatformFailure(_)) {
                *keyring = Keyring::after_failure(e.to_string(), started);
            }
            Err(e).context(what)
        }
    }
}

/// The secret saved under `key`, or `None` when there is none.
pub fn get_secret(key: &str) -> Result<Option<String>> {
    with_entry(
        key,
        "could not read from the OS keyring",
        Entry::get_password,
    )
}

/// Saves `secret` under `key`, or deletes it when `None`.
pub fn set_secret(key: &str, secret: Option<&str>) -> Result<()> {
    match secret {
        Some(secret) => with_entry(key, "could not save to the OS keyring", |entry| {
            entry.set_password(secret)
        }),
        None => with_entry(
            key,
            "could not delete from the OS keyring",
            Entry::delete_credential,
        ),
    }
    .map(drop)
}
