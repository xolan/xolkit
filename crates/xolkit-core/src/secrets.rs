//! Secrets in the OS keyring: the Secret Service on Linux, the Credential Manager on Windows.
//! Nothing secret is written to `~/.xolkit`, so the folder can be backed up or kept in a
//! dotfiles repository.

use anyhow::{Context, Result};

const SERVICE: &str = "xolkit";

/// The keyring entry for the GitHub token typed under Settings.
pub const GITHUB_TOKEN: &str = "github-token";

fn entry(key: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, key).context("could not open the OS keyring")
}

/// The secret saved under `key`, or `None` when there is none.
pub fn get_secret(key: &str) -> Result<Option<String>> {
    match entry(key)?.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e).context("could not read from the OS keyring"),
    }
}

/// Saves `secret` under `key`, or deletes it when `None`.
pub fn set_secret(key: &str, secret: Option<&str>) -> Result<()> {
    let entry = entry(key)?;
    match secret {
        Some(secret) => entry
            .set_password(secret)
            .context("could not save to the OS keyring"),
        None => match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e).context("could not delete from the OS keyring"),
        },
    }
}
