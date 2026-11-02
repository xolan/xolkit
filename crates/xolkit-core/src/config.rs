//! Reading and editing provider config files without disturbing their formatting.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value};
use toml_edit::{DocumentMut, Item, value};

pub(crate) fn read_json(path: &Path) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Value::Object(Map::new())),
        Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
    }
}

pub(crate) fn write_json(path: &Path, json: &Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(json)?;
    text.push('\n');
    write_atomic(path, text.as_bytes())
}

pub(crate) fn read_toml(path: &Path) -> Result<DocumentMut> {
    match fs::read_to_string(path) {
        Ok(text) => text
            .parse()
            .with_context(|| format!("{} is not valid TOML", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
    }
}

pub(crate) fn write_toml(path: &Path, doc: &DocumentMut) -> Result<()> {
    write_atomic(path, doc.to_string().as_bytes())
}

/// Replaces a file in one step, keeping its permissions. Writes through symlinks, so files
/// managed by a dotfile tool stay links.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let path = canonical(path);
    let dir = path
        .parent()
        .context("config file has no parent directory")?;
    fs::create_dir_all(dir)?;
    let name = path.file_name().context("config file has no name")?;
    let tmp = dir.join(format!(".{}.xolkit-tmp", name.to_string_lossy()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // Start private: config files can hold secrets.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("could not write {}", tmp.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    if let Ok(meta) = fs::metadata(&path) {
        fs::set_permissions(&tmp, meta.permissions())?;
    }
    fs::rename(&tmp, &path).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        replace_error(error, &path)
    })
}

/// Windows answers "Access is denied" both for a read-only file and for one another program
/// holds open without letting it be replaced (some editors do; Claude Code and Codex don't).
fn replace_error(error: std::io::Error, path: &Path) -> anyhow::Error {
    let shown = path.display();
    // ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION
    if cfg!(windows) && matches!(error.raw_os_error(), Some(5 | 32)) {
        if fs::metadata(path).is_ok_and(|m| m.permissions().readonly()) {
            return anyhow::anyhow!("could not replace {shown}: it is read-only");
        }
        return anyhow::anyhow!(
            "could not replace {shown}: another program has it open; close that program and try again"
        );
    }
    anyhow::Error::new(error).context(format!("could not replace {shown}"))
}

/// `path` with links resolved, or as given when it does not exist.
fn canonical(path: &Path) -> std::path::PathBuf {
    #[cfg(windows)]
    let resolved = dunce::canonicalize(path).unwrap_or_else(|_| dunce::simplified(path).into());
    #[cfg(not(windows))]
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    resolved
}

/// Turns a plugin Codex has on or off. One Codex doesn't have (no `plugins."<id>"` table) is
/// left alone: there is nothing to switch, and no entry is made for it.
pub(crate) fn set_codex_plugin(doc: &mut DocumentMut, id: &str, enabled: bool) -> Result<()> {
    let Some(plugin) = doc
        .get_mut("plugins")
        .and_then(Item::as_table_like_mut)
        .and_then(|plugins| plugins.get_mut(id))
        .filter(|plugin| !plugin.is_none())
    else {
        return Ok(());
    };
    let plugin = plugin
        .as_table_mut()
        .with_context(|| format!("`plugins.\"{id}\"` in config.toml is not a table"))?;
    plugin["enabled"] = value(enabled);
    Ok(())
}

/// Drops the hook trust entries Codex keeps for a plugin; `codex plugin remove` leaves them.
pub(crate) fn forget_codex_hooks(doc: &mut DocumentMut, id: &str) {
    let Some(state) = doc
        .get_mut("hooks")
        .and_then(|h| h.get_mut("state"))
        .and_then(Item::as_table_like_mut)
    else {
        return;
    };
    let prefix = format!("{id}:");
    let keys: Vec<String> = state
        .iter()
        .map(|(k, _)| k.to_owned())
        .filter(|k| k.starts_with(&prefix))
        .collect();
    for key in keys {
        state.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_plugin_toggle_and_hook_cleanup() {
        let mut doc: DocumentMut = "[plugins.\"p@m\"]\nenabled = true\n\n[hooks.state.\"p@m:hooks/hooks.json:pre:0:0\"]\ntrusted_hash = \"x\"\n\n[hooks.state.\"q@m:h:0\"]\ntrusted_hash = \"y\"\n"
            .parse()
            .unwrap();
        set_codex_plugin(&mut doc, "p@m", false).unwrap();
        forget_codex_hooks(&mut doc, "p@m");
        let text = doc.to_string();
        assert!(text.contains("[plugins.\"p@m\"]\nenabled = false"));
        assert!(!text.contains("p@m:hooks"));
        assert!(text.contains("q@m:h:0"));
        // A plugin Codex doesn't have gets no entry.
        set_codex_plugin(&mut doc, "gone@m", true).unwrap();
        assert!(!doc.to_string().contains("gone@m"));
    }

    #[test]
    fn atomic_write_keeps_permissions_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        fs::write(&real, "a = 1\n").unwrap();
        #[cfg(unix)]
        let link = dir.path().join("config.toml");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
            std::os::unix::fs::symlink(&real, &link).unwrap();
        }
        // File symlinks need Developer Mode on Windows; write the file itself there.
        #[cfg(not(unix))]
        let link = real.clone();
        write_atomic(&link, b"a = 2\n").unwrap();
        assert_eq!(fs::read_to_string(&real).unwrap(), "a = 2\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(
                fs::metadata(&real).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn atomic_write_says_when_another_program_holds_the_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, "{}").unwrap();
        // Shares reading and writing but not deleting, like some editors.
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(1 | 2)
            .open(&path)
            .unwrap();
        let error = write_atomic(&path, b"{\"a\":1}").unwrap_err().to_string();
        assert!(error.contains("another program has it open"), "{error}");
        assert!(!error.contains(r"\\?\"), "{error}");
        drop(held);
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            1,
            "temporary file left"
        );
        write_atomic(&path, b"{\"a\":1}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"a\":1}");
    }
}
