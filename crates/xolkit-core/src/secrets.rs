//! Secrets in the OS keyring: the Secret Service on Linux, the Credential Manager on Windows.
//! Nothing secret is written to `~/.xolkit`, so the folder can be backed up or kept in a
//! dotfiles repository.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use keyring_core::{Entry, Error};

const SERVICE: &str = "xolkit";

/// The keyring entry for the GitHub token typed under Settings.
pub const GITHUB_TOKEN: &str = "github-token";

/// Whether the OS keyring has been reached. While it has not, uses try again, so a keyring that
/// starts or comes back after xolkit did is found without restarting xolkit.
enum Keyring {
    Unreached,
    /// It failed, for the reason `why`, and is not tried again before `until`.
    Failed {
        until: Instant,
        why: String,
    },
    Reached,
}

static KEYRING: Mutex<Keyring> = Mutex::new(Keyring::Unreached);

/// The least time a failure of the keyring stands before it is tried again.
const RETRY_AFTER: Duration = Duration::from_secs(3);

impl Keyring {
    /// The keyring having failed for the reason `why`, in an attempt begun at `started`. The
    /// failure stands for as long as the attempt took, if that is longer than [`RETRY_AFTER`]:
    /// a keyring that hangs takes many seconds to fail, callers ask for one secret after
    /// another, and they should not spend most of their time waiting on it.
    fn failed(why: String, started: Instant) -> Self {
        let until = Instant::now() + started.elapsed().max(RETRY_AFTER);
        Keyring::Failed { until, why }
    }
}

fn keyring() -> MutexGuard<'static, Keyring> {
    KEYRING.lock().unwrap_or_else(PoisonError::into_inner)
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

fn entry(key: &str) -> Result<Entry> {
    {
        let mut keyring = keyring();
        match &*keyring {
            Keyring::Reached => {}
            Keyring::Failed { until, why } if Instant::now() < *until => bail!("{why}"),
            _ => {
                let started = Instant::now();
                match store() {
                    Ok(store) => {
                        keyring_core::set_default_store(store);
                        *keyring = Keyring::Reached;
                    }
                    Err(e) => {
                        let why = format!("{UNREACHABLE}: {e}");
                        *keyring = Keyring::failed(why.clone(), started);
                        bail!(why);
                    }
                }
            }
        }
    }
    Entry::new(SERVICE, key).context("could not open the OS keyring")
}

/// Has the keyring reached for anew, after a while, when the `error` of a use begun at `started`
/// may mean the connection to it was lost, as when the Secret Service provider was restarted.
/// A keyring that is locked is still connected.
fn lost(error: &Error, started: Instant) {
    if matches!(error, Error::PlatformFailure(_)) {
        *keyring() = Keyring::failed(format!("the OS keyring failed: {error}"), started);
    }
}

/// The secret saved under `key`, or `None` when there is none.
pub fn get_secret(key: &str) -> Result<Option<String>> {
    let (entry, started) = (entry(key)?, Instant::now());
    match entry.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(Error::NoEntry) => Ok(None),
        Err(e) => {
            lost(&e, started);
            Err(e).context("could not read from the OS keyring")
        }
    }
}

/// Saves `secret` under `key`, or deletes it when `None`.
pub fn set_secret(key: &str, secret: Option<&str>) -> Result<()> {
    let (entry, started) = (entry(key)?, Instant::now());
    match secret {
        Some(secret) => entry
            .set_password(secret)
            .inspect_err(|e| lost(e, started))
            .context("could not save to the OS keyring"),
        None => match entry.delete_credential() {
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(e) => {
                lost(&e, started);
                Err(e).context("could not delete from the OS keyring")
            }
        },
    }
}
