//! Changes are planned as a list of [`Action`]s, shown to the user, then executed by the daemon,
//! the only process that writes to `~/.xolkit`.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use toml_edit::DocumentMut;

use crate::config::{forget_codex_hooks, read_toml, set_codex_plugin, write_atomic, write_toml};
use crate::lock::{Lock, Pin, PluginPin};
use crate::servers::{read_servers, write_servers};
use crate::{Env, Provider, ServerConfig, Settings};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum Action {
    /// Deletes a file or folder, or unlinks a link. Never follows links.
    Remove {
        path: PathBuf,
    },
    /// Copies a folder that holds no links.
    Copy {
        from: PathBuf,
        to: PathBuf,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
    /// Swaps a downloaded folder into place and runs its refresh steps. Restores the installed
    /// folder if the move or any refresh step fails; the last step commits the new pin.
    ReplaceDir {
        from: PathBuf,
        to: PathBuf,
        backup: PathBuf,
        steps: Vec<Action>,
    },
    WriteFile {
        path: PathBuf,
        contents: String,
    },
    /// Records where a skill came from, or forgets it.
    PinSkill {
        name: String,
        pin: Option<Pin>,
    },
    PinPlugin {
        id: String,
        pin: Option<PluginPin>,
    },
    /// Records whether an agent has a plugin installed natively.
    SetNative {
        id: String,
        provider: Provider,
        installed: bool,
    },
    /// Adds, replaces or removes a server in `servers.toml`.
    SaveServer {
        name: String,
        config: Option<ServerConfig>,
    },
    SaveSettings {
        settings: Box<Settings>,
    },
    SetCodexPlugin {
        id: String,
        enabled: bool,
    },
    ForgetCodexHooks {
        id: String,
    },
    /// Runs an agent's command-line tool.
    Run {
        provider: Provider,
        args: Vec<String>,
    },
    /// Turns a plugin on or off in Claude Code. Claude refuses to turn on what is on already, so
    /// a failure counts as done when the plugin is as wanted, or no longer installed.
    SetClaudePlugin {
        id: String,
        enabled: bool,
    },
    /// Takes xolkit's router skill out of an agent's skills folder: its own files only, checked
    /// when it runs, and the folder if nothing else is left in it.
    RemoveRouter {
        provider: Provider,
    },
    /// Runs an agent's tool to take something out. A failure counts as done when the agent no
    /// longer has `gone`: it was removed there already, which mustn't block removing it here.
    Undo {
        provider: Provider,
        args: Vec<String>,
        gone: Gone,
    },
}

/// What an [`Action::Undo`] takes out of an agent, to check whether it is gone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Gone {
    /// xolkit's MCP server entry.
    McpEntry,
    /// A plugin, by the id the agent knows it by.
    Plugin { id: String },
    /// xolkit's marketplace. Left behind it does no harm, so its removal never fails a plan.
    Marketplace,
}

impl Gone {
    /// Whether the agent no longer has it. Only a clear answer counts: a config or listing that
    /// can't be read means "maybe not".
    fn is_gone(&self, env: &Env, provider: Provider) -> bool {
        match self {
            // The config decides, never the tool: it can be read, and edited, without it.
            Gone::McpEntry => crate::registration::has_entry(env, provider).is_ok_and(|has| !has),
            // An uninstalled agent runs nothing that's left of it.
            _ if agent_absent(env, provider) => true,
            Gone::Plugin { id } => {
                crate::marketplace::native_state(env, provider, id).is_ok_and(|s| s.is_none())
            }
            Gone::Marketplace => true,
        }
    }
}

/// Whether the agent is uninstalled: nothing of it is left, or its tool is missing from the
/// user's whole `PATH` as it is now (uninstalling leaves the folder behind): from the login
/// shell, or on Windows from the registry, since the daemon's own `PATH` dates from its start.
/// When that `PATH` can't be read, a missing tool proves nothing.
fn agent_absent(env: &Env, provider: Provider) -> bool {
    // A missing folder proves nothing either: a fresh install may not have made it yet.
    env.tool_gone(provider.cli())
}

impl Action {
    /// One line for the confirm dialog.
    pub fn describe(&self, env: &Env) -> String {
        let show = |p: &Path| env.tilde(p);
        let on_off = |enabled: &bool| if *enabled { "on" } else { "off" };
        match self {
            Action::Remove { path } if path.starts_with(env.staging_dir()) => {
                "Delete the old copy".to_owned()
            }
            Action::Remove { path } => format!("Delete {}", show(path)),
            Action::Copy { from, to } => format!("Copy {} to {}", show(from), show(to)),
            Action::Rename { from, to } if from.starts_with(env.staging_dir()) => {
                format!("Move the download to {}", show(to))
            }
            Action::Rename { from, .. } => format!("Move {} aside", show(from)),
            Action::ReplaceDir { to, steps, .. } => {
                let mut lines = vec![format!(
                    "Replace {} (restore the old copy if the refresh fails)",
                    show(to)
                )];
                lines.extend(steps.iter().map(|step| step.describe(env)));
                lines.join("\n")
            }
            Action::WriteFile { path, .. } => format!("Write {}", show(path)),
            Action::PinSkill { name, pin: Some(_) }
            | Action::PinPlugin {
                id: name,
                pin: Some(_),
            } => {
                format!("Record {name} in {}", show(&env.lock_file()))
            }
            Action::PinSkill { name, pin: None }
            | Action::PinPlugin {
                id: name,
                pin: None,
            } => {
                format!("Remove {name} from {}", show(&env.lock_file()))
            }
            Action::SetNative {
                id,
                provider,
                installed,
            } => format!(
                "Record that {} {} {id}",
                provider.name(),
                if *installed { "has" } else { "no longer has" }
            ),
            Action::SaveServer {
                name,
                config: Some(_),
            } => format!("Save server {name} in {}", show(&env.servers_file())),
            Action::SaveServer { name, config: None } => {
                format!("Remove server {name} from {}", show(&env.servers_file()))
            }
            Action::SaveSettings { .. } => format!("Save {}", show(&env.settings_file())),
            Action::RemoveRouter { provider } => {
                format!("Remove the router skill from {}", provider.name())
            }
            Action::SetClaudePlugin { id, enabled } => {
                format!("Turn {} plugin {id} in Claude Code", on_off(enabled))
            }
            Action::SetCodexPlugin { id, enabled } => format!(
                "Turn {} plugin {id} in {}",
                on_off(enabled),
                show(&env.codex_config())
            ),
            Action::ForgetCodexHooks { id } => format!(
                "Remove hook trust entries for {id} from {}",
                show(&env.codex_config())
            ),
            Action::Run { provider, args } => {
                format!("Run `{} {}`", provider.cli(), args.join(" "))
            }
            Action::Undo { provider, args, .. } => format!(
                "Run `{} {}`, if it's still there",
                provider.cli(),
                args.join(" ")
            ),
        }
    }
}

/// Runs actions in order and stops at the first failure.
pub fn execute(env: &Env, actions: &[Action]) -> Result<()> {
    for action in actions {
        run(env, action).with_context(|| action.describe(env))?;
    }
    Ok(())
}

fn run(env: &Env, action: &Action) -> Result<()> {
    match action {
        Action::Remove { path } => remove(path),
        Action::Copy { from, to } => {
            fs::create_dir_all(to.parent().context("target has no parent directory")?)?;
            copy_dir(from, to)
        }
        Action::Rename { from, to } => {
            fs::create_dir_all(to.parent().context("target has no parent directory")?)?;
            Ok(fs::rename(from, to)?)
        }
        Action::ReplaceDir {
            from,
            to,
            backup,
            steps,
        } => {
            anyhow::ensure!(
                fs::symlink_metadata(to).is_ok_and(|m| m.is_dir()),
                "{} is not a folder xolkit can replace",
                to.display()
            );
            fs::create_dir_all(backup.parent().context("backup has no parent directory")?)?;
            fs::rename(to, backup)?;
            let refresh = fs::rename(from, to)
                .map_err(anyhow::Error::from)
                .and_then(|()| execute(env, steps));
            if let Err(error) = refresh {
                // Preserve the failed download while restoring the previous installed files.
                if to.exists() {
                    fs::rename(to, from).with_context(|| {
                        format!(
                            "could not move the failed update aside; old copy is at {}",
                            backup.display()
                        )
                    })?;
                }
                fs::rename(backup, to).with_context(|| {
                    format!(
                        "could not restore {}; the old copy is at {}",
                        to.display(),
                        backup.display()
                    )
                })?;
                return Err(error);
            }
            Ok(())
        }
        Action::WriteFile { path, contents } => write_atomic(path, contents.as_bytes()),
        Action::PinSkill { name, pin } => Lock::edit(&env.lock_file(), |lock| match pin {
            Some(pin) => {
                lock.skills.insert(name.clone(), pin.clone());
            }
            None => {
                lock.skills.remove(name);
            }
        }),
        Action::PinPlugin { id, pin } => Lock::edit(&env.lock_file(), |lock| match pin {
            Some(pin) => {
                lock.plugins.insert(id.clone(), pin.clone());
            }
            None => {
                lock.plugins.remove(id);
            }
        }),
        Action::SetNative {
            id,
            provider,
            installed,
        } => Lock::edit(&env.lock_file(), |lock| {
            let entry = lock.plugins.entry(id.clone()).or_default();
            if *installed {
                entry.native.insert(*provider);
            } else {
                entry.native.remove(provider);
            }
        }),
        Action::SaveServer { name, config } => {
            let mut servers = read_servers(&env.servers_file())?;
            match config {
                Some(config) => {
                    servers.insert(name.clone(), config.clone());
                }
                None => {
                    servers.remove(name);
                }
            }
            write_servers(&env.servers_file(), &servers)
        }
        Action::SaveSettings { settings } => settings.save(&env.settings_file()),
        Action::SetCodexPlugin { id, enabled } => edit_toml(&env.codex_config(), |doc| {
            set_codex_plugin(doc, id, *enabled)
        }),
        Action::ForgetCodexHooks { id } => edit_toml(&env.codex_config(), |doc| {
            forget_codex_hooks(doc, id);
            Ok(())
        }),
        Action::Run { provider, args } => env.run(provider.cli(), args).map(drop),
        Action::RemoveRouter { provider } => crate::write_router(env, *provider, None),
        Action::SetClaudePlugin { id, enabled } => {
            let verb = if *enabled { "enable" } else { "disable" };
            let args = ["plugin", verb, id, "--scope", "user"].map(String::from);
            match env.run(Provider::Claude.cli(), &args) {
                Ok(_) => Ok(()),
                // Claude Code is gone; nothing of it runs to switch.
                Err(_) if agent_absent(env, Provider::Claude) => Ok(()),
                Err(error) => match crate::marketplace::native_state(env, Provider::Claude, id) {
                    // Already as wanted; or off and not installed at all.
                    Ok(Some(on)) if on == *enabled => Ok(()),
                    Ok(None) if !*enabled => Ok(()),
                    Ok(None) => Err(anyhow::anyhow!(
                        "Claude Code no longer has the plugin {id}; install it natively again"
                    )),
                    _ => Err(error),
                },
            }
        }
        Action::Undo {
            provider,
            args,
            gone,
        } => match env.run(provider.cli(), args) {
            Ok(_) => Ok(()),
            Err(_) if gone.is_gone(env, *provider) => Ok(()),
            // The agent's tool couldn't take xolkit's own entry out; take it out of the config.
            Err(error) if *gone == Gone::McpEntry => {
                crate::registration::remove_entry_from_config(env, *provider)
                    .with_context(|| format!("{error:#}"))?;
                anyhow::ensure!(gone.is_gone(env, *provider), error);
                Ok(())
            }
            Err(error) => Err(error),
        },
    }
}

fn remove(path: &Path) -> Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    // symlink_metadata never reports a link as a directory, so links are unlinked, not followed.
    if meta.is_dir() {
        fs::remove_dir_all(path)?;
    } else if is_dir_link(&meta) {
        // Windows unlinks a junction or directory symlink like an empty directory.
        fs::remove_dir(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Copies the folder `from` to `to`, which must not exist yet. Refuses links inside `from`, so a
/// copy never reaches outside it.
pub(crate) fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir(to).with_context(|| format!("could not create {}", to.display()))?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let to = to.join(entry.file_name());
        if kind.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &to)?;
        } else {
            anyhow::bail!("{} is a link; refusing to copy it", entry.path().display());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_dir_link(meta: &fs::Metadata) -> bool {
    std::os::windows::fs::FileTypeExt::is_symlink_dir(&meta.file_type())
}

#[cfg(not(windows))]
fn is_dir_link(_meta: &fs::Metadata) -> bool {
    false
}

#[cfg(all(test, unix))]
pub(crate) fn link_dir(target: &Path, link: &Path) -> Result<()> {
    Ok(std::os::unix::fs::symlink(target, link)?)
}

#[cfg(all(test, windows))]
pub(crate) fn link_dir(target: &Path, link: &Path) -> Result<()> {
    Ok(junction::create(target, link)?)
}

/// Rewrites a TOML file only when the edit changes it.
fn edit_toml(path: &Path, edit: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    let mut doc = read_toml(path)?;
    let before = doc.to_string();
    edit(&mut doc)?;
    if doc.to_string() != before {
        write_toml(path, &doc)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Agent, Kind};

    #[test]
    fn actions_travel_as_json() {
        let mut settings = Settings::default();
        settings
            .active_mut()
            .set(Kind::Skill, "s", Agent::Claude, true);
        let actions = vec![
            Action::Remove { path: "/x".into() },
            Action::SaveSettings {
                settings: Box::new(settings),
            },
            Action::Run {
                provider: Provider::Codex,
                args: vec!["plugin".into()],
            },
        ];
        let json = serde_json::to_string(&actions).unwrap();
        assert!(json.contains(r#""action":"remove""#), "{json}");
        let back: Vec<Action> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, actions);
    }

    #[test]
    fn failed_replacement_restores_the_installed_folder() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let target = home.path().join("installed");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), "old").unwrap();
        let backup = env.staging_dir().join("backup");
        let result = execute(
            &env,
            &[Action::ReplaceDir {
                from: home.path().join("missing-download"),
                to: target.clone(),
                backup: backup.clone(),
                steps: Vec::new(),
            }],
        );
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(target.join("keep")).unwrap(), "old");
        assert!(!backup.exists());
    }

    #[test]
    fn removes_links_without_following_them() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), "x").unwrap();
        let link = dir.path().join("link");
        crate::testing::link_dir(&target, &link);
        remove(&link).unwrap();
        assert!(!link.exists() && target.join("keep").exists());
        remove(&dir.path().join("missing")).unwrap();
    }

    #[test]
    fn saves_servers_settings_and_native_state() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let config = ServerConfig {
            command: Some("srv".into()),
            ..ServerConfig::default()
        };
        let mut settings = Settings::default();
        settings.set("theme", "x");
        execute(
            &env,
            &[
                Action::SaveServer {
                    name: "a".into(),
                    config: Some(config.clone()),
                },
                Action::SetNative {
                    id: "p@m".into(),
                    provider: Provider::Claude,
                    installed: true,
                },
                Action::SaveSettings {
                    settings: Box::new(settings.clone()),
                },
            ],
        )
        .unwrap();
        assert_eq!(read_servers(&env.servers_file()).unwrap()["a"], config);
        let lock = Lock::read(&env.lock_file()).unwrap();
        assert!(lock.native("p@m").contains(&Provider::Claude));
        assert_eq!(Settings::load(&env.settings_file()).unwrap(), settings);
        execute(
            &env,
            &[Action::SaveServer {
                name: "a".into(),
                config: None,
            }],
        )
        .unwrap();
        assert!(read_servers(&env.servers_file()).unwrap().is_empty());
    }
}
