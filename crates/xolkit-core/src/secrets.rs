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
    /// It was slow to say, with `why`, that it cannot be reached, and is not tried again
    /// before `until`.
    Unreachable {
        until: Instant,
        why: String,
    },
    Reached,
}

/// Held for the whole of each use, so uses take turns across the process. A use that waits on
/// the keyring, for the user to unlock it or for a provider that never answers, keeps every
/// other use waiting with it.
static KEYRING: Mutex<Keyring> = Mutex::new(Keyring::Unreached);

/// How long reaching the keyring may take to fail before it is left alone for a while, and the
/// longest it is left alone. A provider that fails to start can take many seconds to say so,
/// and callers ask for one secret after another: they should not each wait that out. Only
/// reaching the keyring is treated so. A read or write that fails, however slowly, may have
/// waited on the user, and its caller may have work to undo, so the next use goes ahead.
const SLOW: Duration = Duration::from_secs(1);
const LONGEST_PAUSE: Duration = Duration::from_secs(10);

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
    if let Keyring::Unreachable { until, why } = &*keyring
        && Instant::now() < *until
    {
        return Err(anyhow!("{why}")).context(what);
    }
    if !matches!(*keyring, Keyring::Reached) {
        let started = Instant::now();
        match store() {
            Ok(store) => {
                keyring_core::set_default_store(store);
                *keyring = Keyring::Reached;
            }
            Err(e) => {
                let (why, took) = (format!("{UNREACHABLE}: {e}"), started.elapsed());
                *keyring = if took < SLOW {
                    Keyring::Unreached
                } else {
                    Keyring::Unreachable {
                        until: Instant::now() + took.min(LONGEST_PAUSE),
                        why: why.clone(),
                    }
                };
                return Err(anyhow!(why)).context(what);
            }
        }
    }
    match Entry::new(SERVICE, key).and_then(|entry| op(&entry)) {
        Ok(value) => Ok(Some(value)),
        Err(Error::NoEntry) => Ok(None),
        Err(e) => {
            // The connection may be lost, as when a Secret Service provider was restarted, so
            // the next use reaches for the keyring anew. One that is locked is still connected.
            if matches!(e, Error::PlatformFailure(_)) {
                *keyring = Keyring::Unreached;
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
