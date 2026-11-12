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
    /// Not reached when last tried, at `at`, for the reason `why`.
    Failed {
        at: Instant,
        why: String,
    },
    Reached,
}

static KEYRING: Mutex<Keyring> = Mutex::new(Keyring::Unreached);

/// How long a failure to reach the keyring stands before trying again. Reaching for a keyring
/// that hangs can take many seconds, and callers ask for one secret after another.
const RETRY_AFTER: Duration = Duration::from_secs(3);

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
            Keyring::Failed { at, why } if at.elapsed() < RETRY_AFTER => bail!("{why}"),
            _ => match store() {
                Ok(store) => {
                    keyring_core::set_default_store(store);
                    *keyring = Keyring::Reached;
                }
                Err(e) => {
                    let why = format!("{UNREACHABLE}: {e}");
                    *keyring = Keyring::Failed {
                        at: Instant::now(),
                        why: why.clone(),
                    };
                    bail!(why);
                }
            },
        }
    }
    Entry::new(SERVICE, key).context("could not open the OS keyring")
}

/// Has the next use reach for the keyring anew when `error` may mean the connection to it was
/// lost, as when the Secret Service provider was restarted.
fn lost(error: &Error) {
    if matches!(error, Error::PlatformFailure(_) | Error::NoStorageAccess(_)) {
        *keyring() = Keyring::Unreached;
    }
}

/// The secret saved under `key`, or `None` when there is none.
pub fn get_secret(key: &str) -> Result<Option<String>> {
    match entry(key)?.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(Error::NoEntry) => Ok(None),
        Err(e) => {
            lost(&e);
            Err(e).context("could not read from the OS keyring")
        }
    }
}

/// Saves `secret` under `key`, or deletes it when `None`.
pub fn set_secret(key: &str, secret: Option<&str>) -> Result<()> {
    let entry = entry(key)?;
    match secret {
        Some(secret) => entry
            .set_password(secret)
            .inspect_err(lost)
            .context("could not save to the OS keyring"),
        None => match entry.delete_credential() {
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(e) => {
                lost(&e);
                Err(e).context("could not delete from the OS keyring")
            }
        },
    }
}
