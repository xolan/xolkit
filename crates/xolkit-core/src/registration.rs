//! Registering xolkit with an agent: one MCP server entry, written once by the agent's own
//! tool, that starts `xolkit mcp`. Plus the optional router skill.

use std::path::{Path, PathBuf};

use crate::config::{read_json, read_toml};
use crate::lock::Lock;
use crate::marketplace::MARKETPLACE;
use crate::{Action, Env, Provider, Settings, XOLKIT};

/// How xolkit is registered with an agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Registration {
    /// No `xolkit` server in the agent's config.
    Missing,
    /// Registered to start this program.
    Current,
    /// Registered, but to start another program (such as an older copy).
    Other(String),
}

/// The command line an agent starts xolkit with.
fn command_line(exe: &Path, provider: Provider) -> Vec<String> {
    vec![
        exe.to_string_lossy().into_owned(),
        "mcp".to_owned(),
        "--agent".to_owned(),
        provider.cli().to_owned(),
    ]
}

/// How xolkit is registered with `provider`, read from the agent's config file.
pub fn registration(env: &Env, provider: Provider, exe: &Path) -> Registration {
    let command = match provider {
        Provider::Claude => read_json(&env.claude_json()).ok().and_then(|json| {
            json["mcpServers"][XOLKIT]["command"]
                .as_str()
                .map(str::to_owned)
        }),
        Provider::Codex => read_toml(&env.codex_config()).ok().and_then(|doc| {
            doc.get("mcp_servers")?
                .get(XOLKIT)?
                .get("command")?
                .as_str()
                .map(str::to_owned)
        }),
    };
    match command {
        None => Registration::Missing,
        Some(command) if Path::new(&command) == exe => Registration::Current,
        Some(command) => Registration::Other(command),
    }
}

/// Steps that register xolkit with `provider`, replacing an entry that starts another program.
pub fn register(env: &Env, provider: Provider, exe: &Path) -> Vec<Action> {
    let mut actions = Vec::new();
    if registration(env, provider, exe) != Registration::Missing {
        actions.push(remove_entry(provider));
    }
    let start: &[&str] = match provider {
        Provider::Claude => &["mcp", "add", "--scope", "user", XOLKIT, "--"],
        Provider::Codex => &["mcp", "add", XOLKIT, "--"],
    };
    let mut args: Vec<String> = start.iter().map(|a| (*a).to_owned()).collect();
    args.extend(command_line(exe, provider));
    actions.push(Action::Run { provider, args });
    actions
}

fn remove_entry(provider: Provider) -> Action {
    let args: &[&str] = match provider {
        Provider::Claude => &["mcp", "remove", "--scope", "user", XOLKIT],
        Provider::Codex => &["mcp", "remove", XOLKIT],
    };
    Action::Run {
        provider,
        args: args.iter().map(|a| (*a).to_owned()).collect(),
    }
}

/// Where the router skill goes for `provider`.
pub(crate) fn router_dir(env: &Env, provider: Provider) -> PathBuf {
    match provider {
        Provider::Claude => env.claude_skills(),
        Provider::Codex => env.codex_skills(),
    }
    .join(XOLKIT)
}

/// Writes the router skill for `provider`, or removes it when `skill` is `None`. Leaves the file
/// alone when it already says the same, so the agent's file watcher has nothing to report.
pub fn write_router(env: &Env, provider: Provider, skill: Option<&str>) -> anyhow::Result<()> {
    let dir = router_dir(env, provider);
    let path = dir.join("SKILL.md");
    match skill {
        Some(text) => {
            if std::fs::read_to_string(&path).is_ok_and(|old| old == text) {
                return Ok(());
            }
            crate::config::write_atomic(&path, text.as_bytes())
        }
        None => match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        },
    }
}

/// Steps that take everything xolkit wrote out of `provider`: its MCP server entry, the router
/// skill (turned off first, so the daemon doesn't write it again), and the plugins it installed
/// natively, with xolkit's marketplace.
pub fn unregister(
    env: &Env,
    provider: Provider,
    exe: &Path,
    lock: &Lock,
    settings: &Settings,
) -> Vec<Action> {
    let mut actions = Vec::new();
    if registration(env, provider, exe) != Registration::Missing {
        actions.push(remove_entry(provider));
    }
    if settings.router.contains(&provider) {
        let mut settings = settings.clone();
        settings.router.retain(|p| *p != provider);
        actions.push(Action::SaveSettings {
            settings: Box::new(settings),
        });
    }
    let router = router_dir(env, provider);
    if router.exists() {
        actions.push(Action::Remove { path: router });
    }
    let run = |args: &[&str]| Action::Run {
        provider,
        args: args.iter().map(|a| (*a).to_owned()).collect(),
    };
    let mut any_native = false;
    for (id, pin) in &lock.plugins {
        if !pin.native.contains(&provider) {
            continue;
        }
        any_native = true;
        let name = id.split_once('@').map_or(id.as_str(), |(name, _)| name);
        let native = format!("{name}@{MARKETPLACE}");
        actions.push(match provider {
            Provider::Claude => run(&["plugin", "uninstall", &native, "--scope", "user"]),
            Provider::Codex => run(&["plugin", "remove", &native]),
        });
        actions.push(Action::SetNative {
            id: id.clone(),
            provider,
            installed: false,
        });
    }
    if any_native {
        actions.push(run(&["plugin", "marketplace", "remove", MARKETPLACE]));
    }
    actions
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use super::*;
    use crate::PluginPin;

    #[test]
    fn reads_registrations_from_agent_configs() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let exe = home.path().join("bin/xolkit");
        assert_eq!(
            registration(&env, Provider::Claude, &exe),
            Registration::Missing
        );
        fs::write(
            env.claude_json(),
            format!(
                r#"{{"mcpServers": {{"xolkit": {{"command": {:?}}}}}}}"#,
                exe.to_string_lossy()
            ),
        )
        .unwrap();
        assert_eq!(
            registration(&env, Provider::Claude, &exe),
            Registration::Current
        );
        fs::create_dir_all(&env.codex_dir).unwrap();
        fs::write(
            env.codex_config(),
            "[mcp_servers.xolkit]\ncommand = \"/old/xolkit\"\n",
        )
        .unwrap();
        assert_eq!(
            registration(&env, Provider::Codex, &exe),
            Registration::Other("/old/xolkit".into())
        );
    }

    #[test]
    fn registers_through_the_agents_tools() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let exe = Path::new("/usr/bin/xolkit");
        let line = |a: &Action| match a {
            Action::Run { provider, args } => format!("{} {}", provider.cli(), args.join(" ")),
            other => format!("{other:?}"),
        };
        let claude: Vec<_> = register(&env, Provider::Claude, exe)
            .iter()
            .map(line)
            .collect();
        assert_eq!(
            claude,
            ["claude mcp add --scope user xolkit -- /usr/bin/xolkit mcp --agent claude"]
        );
        fs::create_dir_all(&env.codex_dir).unwrap();
        fs::write(
            env.codex_config(),
            "[mcp_servers.xolkit]\ncommand = \"/old\"\n",
        )
        .unwrap();
        let codex: Vec<_> = register(&env, Provider::Codex, exe)
            .iter()
            .map(line)
            .collect();
        assert_eq!(
            codex,
            [
                "codex mcp remove xolkit",
                "codex mcp add xolkit -- /usr/bin/xolkit mcp --agent codex"
            ]
        );
    }

    #[test]
    fn unregistering_takes_out_everything_xolkit_wrote() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let exe = Path::new("/usr/bin/xolkit");
        fs::create_dir_all(router_dir(&env, Provider::Claude)).unwrap();
        fs::write(
            env.claude_json(),
            r#"{"mcpServers": {"xolkit": {"command": "x"}}}"#,
        )
        .unwrap();
        let mut lock = Lock::default();
        lock.plugins.insert(
            "hooks@m".into(),
            PluginPin {
                native: BTreeSet::from([Provider::Claude]),
                ..PluginPin::default()
            },
        );
        let settings = Settings {
            router: vec![Provider::Claude],
            ..Settings::default()
        };
        let actions = unregister(&env, Provider::Claude, exe, &lock, &settings);
        assert_eq!(actions.len(), 6, "{actions:?}");
        let Action::SaveSettings { settings: saved } = &actions[1] else {
            panic!("{actions:?}");
        };
        assert!(saved.router.is_empty());
        assert_eq!(
            actions[2],
            Action::Remove {
                path: router_dir(&env, Provider::Claude)
            }
        );
        assert!(unregister(&env, Provider::Codex, exe, &lock, &settings).is_empty());
    }
}
