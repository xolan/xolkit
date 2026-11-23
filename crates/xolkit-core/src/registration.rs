//! Registering xolkit with an agent: one MCP server entry that starts `xolkit mcp`, written
//! through the agent's own tool (or directly to `mcp_config.json` for Antigravity). Plus the
//! optional router skill.

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::config::{read_json, read_toml, write_json, write_toml};
use crate::lock::Lock;
use crate::marketplace::MARKETPLACE;
use crate::ops::Gone;
use crate::{Action, Agent, Env, Provider, Settings, Target, XOLKIT};

/// How xolkit is registered with an agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Registration {
    /// No `xolkit` server in the agent's config.
    Missing,
    /// Registered to start this program for this agent.
    Current,
    /// Registered, but to start another program or a different agent key.
    Other(String),
}

/// The command line an agent starts xolkit with.
fn command_line(exe: &Path, agent: &Agent) -> Vec<String> {
    vec![
        exe.to_string_lossy().into_owned(),
        "mcp".to_owned(),
        "--agent".to_owned(),
        agent.key().to_owned(),
    ]
}

fn json_command_and_args(json: &serde_json::Value) -> Option<(String, Option<Vec<String>>)> {
    let entry = json.get("mcpServers")?.get(XOLKIT)?;
    let command = entry.get("command")?.as_str()?.to_owned();
    let args = entry.get("args").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect()
    });
    Some((command, args))
}

fn args_match_agent(args: &[String], agent: &Agent) -> bool {
    args.windows(2)
        .any(|pair| pair[0] == "--agent" && pair[1] == agent.key())
}

/// How xolkit is registered for `target`, read from its config file.
pub fn registration(env: &Env, target: &Target, exe: &Path) -> Registration {
    let target_env = env.for_target(target);
    let entry = match target.provider {
        Provider::Claude => read_json(&target_env.claude_json())
            .ok()
            .and_then(|json| json_command_and_args(&json)),
        Provider::Codex => read_toml(&target_env.codex_config()).ok().and_then(|doc| {
            let server = doc.get("mcp_servers")?.get(XOLKIT)?;
            let command = server.get("command")?.as_str()?.to_owned();
            let args = server
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| item.as_str().map(str::to_owned))
                        .collect()
                });
            Some((command, args))
        }),
        Provider::Antigravity => read_json(&target_env.antigravity_mcp_config())
            .ok()
            .and_then(|json| json_command_and_args(&json)),
    };
    match entry {
        None => Registration::Missing,
        Some((command, _)) if Path::new(&command) != exe => Registration::Other(command),
        Some((command, Some(args))) if !args.is_empty() && !args_match_agent(&args, &target.agent) => {
            Registration::Other(format!("{command} {}", args.join(" ")))
        }
        Some(_) => Registration::Current,
    }
}

/// Whether `provider`'s config has a `xolkit` server entry. Fails when the config can't be read,
/// so an unreadable file never passes for "no entry".
pub(crate) fn has_entry(env: &Env, provider: Provider) -> anyhow::Result<bool> {
    Ok(match provider {
        Provider::Claude => read_json(&env.claude_json())?["mcpServers"]
            .get(XOLKIT)
            .is_some(),
        Provider::Codex => read_toml(&env.codex_config())?
            .get("mcp_servers")
            .and_then(|servers| servers.get(XOLKIT))
            .is_some(),
        Provider::Antigravity => read_json(&env.antigravity_mcp_config())?["mcpServers"]
            .get(XOLKIT)
            .is_some(),
    })
}

/// Writes xolkit's server entry directly into `provider`'s JSON config (used for Antigravity,
/// which configures MCP servers directly in `mcp_config.json`).
pub(crate) fn write_entry_to_config(
    env: &Env,
    provider: Provider,
    agent: &Agent,
    exe: &Path,
) -> anyhow::Result<()> {
    let path = match provider {
        Provider::Antigravity => env.antigravity_mcp_config(),
        Provider::Claude => env.claude_json(),
        Provider::Codex => anyhow::bail!("use `codex mcp add` to register with Codex"),
    };
    let mut json = read_json(&path)?;
    let root = json
        .as_object_mut()
        .with_context(|| format!("{} must be a JSON object", path.display()))?;
    let servers = root
        .entry("mcpServers")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .with_context(|| format!("`mcpServers` in {} is not an object", path.display()))?;
    servers.insert(
        XOLKIT.to_owned(),
        serde_json::json!({
            "command": exe.to_string_lossy(),
            "args": ["mcp", "--agent", agent.key()],
        }),
    );
    write_json(&path, &json)
}

/// Takes xolkit's server entry out of `provider`'s config directly, for Antigravity or when an
/// agent's own tool can't: it is gone, or fails.
pub(crate) fn remove_entry_from_config(env: &Env, provider: Provider) -> anyhow::Result<()> {
    match provider {
        Provider::Claude | Provider::Antigravity => {
            let path = if provider == Provider::Claude {
                env.claude_json()
            } else {
                env.antigravity_mcp_config()
            };
            let mut json = read_json(&path)?;
            let removed = json
                .get_mut("mcpServers")
                .and_then(|s| s.as_object_mut())
                .is_some_and(|servers| servers.shift_remove(XOLKIT).is_some());
            if removed {
                write_json(&path, &json)?;
            }
        }
        Provider::Codex => {
            let path = env.codex_config();
            let mut doc = read_toml(&path)?;
            let removed = doc
                .get_mut("mcp_servers")
                .and_then(|s| s.as_table_like_mut())
                .is_some_and(|servers| servers.remove(XOLKIT).is_some());
            if removed {
                write_toml(&path, &doc)?;
            }
        }
    }
    Ok(())
}

/// Steps that register xolkit with `target`, replacing an entry that starts another program or
/// another agent key.
pub fn register(env: &Env, target: &Target, exe: &Path) -> Vec<Action> {
    if target.provider == Provider::Antigravity {
        return vec![Action::SetMcpEntry {
            provider: Provider::Antigravity,
            agent: target.agent.clone(),
            exe: exe.to_path_buf(),
            dir: target.custom_dir.clone(),
        }];
    }
    let mut actions = Vec::new();
    if registration(env, target, exe) != Registration::Missing {
        actions.push(remove_entry(target.provider, target.custom_dir.clone()));
    }
    let start: &[&str] = match target.provider {
        Provider::Claude => &["mcp", "add", "--scope", "user", XOLKIT, "--"],
        Provider::Codex => &["mcp", "add", XOLKIT, "--"],
        Provider::Antigravity => unreachable!(),
    };
    let mut args: Vec<String> = start.iter().map(|a| (*a).to_owned()).collect();
    args.extend(command_line(exe, &target.agent));
    actions.push(Action::Run {
        provider: target.provider,
        args,
        dir: target.custom_dir.clone(),
    });
    actions
}

fn remove_entry(provider: Provider, dir: Option<PathBuf>) -> Action {
    if provider == Provider::Antigravity {
        return Action::RemoveMcpEntry { provider, dir };
    }
    let args: &[&str] = match provider {
        Provider::Claude => &["mcp", "remove", "--scope", "user", XOLKIT],
        Provider::Codex => &["mcp", "remove", XOLKIT],
        Provider::Antigravity => unreachable!(),
    };
    Action::Undo {
        provider,
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        gone: Gone::McpEntry,
        dir,
    }
}

/// Where the router skill goes for `provider`.
pub(crate) fn router_dir(env: &Env, provider: Provider) -> PathBuf {
    match provider {
        Provider::Claude => env.claude_skills(),
        Provider::Codex => env.codex_skills(),
        Provider::Antigravity => env.antigravity_skills(),
    }
    .join(XOLKIT)
}

/// Marks a router skill as xolkit's, so a skill of the user's own named `xolkit` is left alone.
pub(crate) const ROUTER_MARK: &str = "This skill is written by xolkit";

/// Whether the router skill in `provider`'s folder is one xolkit wrote.
pub fn router_is_ours(env: &Env, provider: Provider) -> bool {
    std::fs::read_to_string(router_dir(env, provider).join("SKILL.md"))
        .is_ok_and(|text| text.contains(ROUTER_MARK))
}

/// The files of xolkit's router skill: the skill, and a write of it cut short.
const ROUTER_FILES: [&str; 2] = ["SKILL.md", ".SKILL.md.xolkit-tmp"];

/// Whether the router's folder holds nothing of the user's: it is xolkit's, missing, or empty
/// but for a write of xolkit's cut short. Only then may xolkit write or remove it.
fn router_free(env: &Env, provider: Provider) -> bool {
    let dir = router_dir(env, provider);
    if !dir.exists() || router_is_ours(env, provider) {
        return true;
    }
    std::fs::read_dir(&dir)
        .is_ok_and(|entries| entries.flatten().all(|e| e.file_name() == ROUTER_FILES[1]))
}

/// Writes the router skill for `provider`, or removes it when `skill` is `None`. Leaves the file
/// alone when it already says the same, so the agent's file watcher has nothing to report, and
/// never touches a skill named `xolkit` that xolkit didn't write.
pub fn write_router(env: &Env, provider: Provider, skill: Option<&str>) -> anyhow::Result<()> {
    let dir = router_dir(env, provider);
    let path = dir.join("SKILL.md");
    if !router_free(env, provider) {
        anyhow::ensure!(
            skill.is_none(),
            "{} holds a skill of your own; rename it to use the router skill",
            env.tilde(&dir)
        );
        return Ok(());
    }
    match skill {
        Some(text) => {
            if std::fs::read_to_string(&path).is_ok_and(|old| old == text) {
                return Ok(());
            }
            crate::config::write_atomic(&path, text.as_bytes())
        }
        None => {
            // xolkit's own files only; anything the user put beside them stays, with the folder.
            for file in ROUTER_FILES {
                match std::fs::remove_file(dir.join(file)) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                    _ => {}
                }
            }
            let _ = std::fs::remove_dir(&dir);
            Ok(())
        }
    }
}

/// Steps that take everything xolkit wrote out of `target`: its MCP server entry, the router
/// skill (turned off first, so the daemon doesn't write it again), and the plugins it installed
/// natively, with xolkit's marketplace.
pub fn unregister(env: &Env, target: &Target, lock: &Lock, settings: &Settings) -> Vec<Action> {
    let target_env = env.for_target(target);
    let mut actions = Vec::new();
    // A config that can't be read may still have the entry: try, and let the check decide.
    if has_entry(&target_env, target.provider).unwrap_or(true) {
        actions.push(remove_entry(target.provider, target.custom_dir.clone()));
    }
    if settings.router.contains(&target.agent) {
        let mut settings = settings.clone();
        settings.router.retain(|a| a != &target.agent);
        actions.push(Action::SaveSettings {
            settings: Box::new(settings),
        });
    }
    let router = router_dir(&target_env, target.provider);
    if router.exists() && router_free(&target_env, target.provider) {
        actions.push(Action::RemoveRouter {
            provider: target.provider,
            dir: target.custom_dir.clone(),
        });
    }
    if target.provider.supports_native() {
        let run = |args: &[&str], gone: Gone| Action::Undo {
            provider: target.provider,
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            gone,
            dir: target.custom_dir.clone(),
        };
        let mut any_native = false;
        for (id, pin) in &lock.plugins {
            if !pin.native.contains(&target.agent) {
                continue;
            }
            any_native = true;
            let name = id.split_once('@').map_or(id.as_str(), |(name, _)| name);
            let native = format!("{name}@{MARKETPLACE}");
            let gone = Gone::Plugin { id: native.clone() };
            actions.push(match target.provider {
                Provider::Claude => run(&["plugin", "uninstall", &native, "--scope", "user"], gone),
                Provider::Codex => run(&["plugin", "remove", &native], gone),
                Provider::Antigravity => unreachable!(),
            });
            actions.push(Action::SetNative {
                id: id.clone(),
                agent: target.agent.clone(),
                installed: false,
            });
        }
        if any_native {
            actions.push(run(
                &["plugin", "marketplace", "remove", MARKETPLACE],
                Gone::Marketplace,
            ));
        }
    }
    actions
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use super::*;
    use crate::{CustomAgent, PluginPin, execute};

    #[test]
    fn reads_registrations_from_agent_configs() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let exe = home.path().join("bin/xolkit");
        let claude = Target::builtin(&env, Provider::Claude);
        let codex = Target::builtin(&env, Provider::Codex);
        let ag = Target::builtin(&env, Provider::Antigravity);
        assert_eq!(registration(&env, &claude, &exe), Registration::Missing);
        assert_eq!(registration(&env, &ag, &exe), Registration::Missing);
        fs::write(
            env.claude_json(),
            format!(
                r#"{{"mcpServers": {{"xolkit": {{"command": {:?}}}}}}}"#,
                exe.to_string_lossy()
            ),
        )
        .unwrap();
        assert_eq!(registration(&env, &claude, &exe), Registration::Current);
        fs::create_dir_all(&env.codex_dir).unwrap();
        fs::write(
            env.codex_config(),
            "[mcp_servers.xolkit]\ncommand = \"/old/xolkit\"\n",
        )
        .unwrap();
        assert_eq!(
            registration(&env, &codex, &exe),
            Registration::Other("/old/xolkit".into())
        );

        // Antigravity registers directly into mcp_config.json and preserves other servers.
        fs::create_dir_all(&env.antigravity_dir).unwrap();
        fs::write(
            env.antigravity_mcp_config(),
            r#"{"mcpServers": {"other": {"command": "node"}}}"#,
        )
        .unwrap();
        let steps = register(&env, &ag, &exe);
        execute(&env, &steps).unwrap();
        assert_eq!(registration(&env, &ag, &exe), Registration::Current);
        assert!(
            fs::read_to_string(env.antigravity_mcp_config())
                .unwrap()
                .contains("other")
        );

        // A cloned config with the wrong `--agent` key reports `Other`.
        let custom = CustomAgent {
            id: "claude-two".into(),
            name: "Claude (two)".into(),
            provider: Provider::Claude,
            dir: PathBuf::from("~/.claude-two"),
        };
        let target = Target::custom(&env, &custom);
        fs::create_dir_all(&target.dir).unwrap();
        fs::write(
            target.dir.join(".claude.json"),
            format!(
                r#"{{"mcpServers": {{"xolkit": {{"command": {:?}, "args": ["mcp", "--agent", "claude"]}}}}}}"#,
                exe.to_string_lossy()
            ),
        )
        .unwrap();
        assert!(matches!(
            registration(&env, &target, &exe),
            Registration::Other(_)
        ));
    }

    #[test]
    fn leaves_a_users_own_xolkit_folder_alone() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let dir = router_dir(&env, Provider::Claude);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notes.md"), "mine").unwrap();
        let settings = Settings::default();
        let lock = Lock::default();
        let claude = Target::builtin(&env, Provider::Claude);
        // No SKILL.md, but files of the user's: neither removed nor written over.
        assert!(unregister(&env, &claude, &lock, &settings).is_empty());
        assert!(write_router(&env, Provider::Claude, Some("x")).is_err());
        write_router(&env, Provider::Claude, None).unwrap();
        assert!(dir.join("notes.md").exists());
        // An empty leftover is xolkit's to use.
        fs::remove_file(dir.join("notes.md")).unwrap();
        write_router(&env, Provider::Claude, Some(ROUTER_MARK)).unwrap();
        assert!(router_is_ours(&env, Provider::Claude));
    }

    #[test]
    fn removes_its_own_entry_from_agent_configs() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        fs::write(
            env.claude_json(),
            r#"{"mcpServers": {"xolkit": {"command": "x"}, "other": {"command": "y"}}}"#,
        )
        .unwrap();
        fs::create_dir_all(&env.codex_dir).unwrap();
        fs::write(
            env.codex_config(),
            "[mcp_servers.xolkit]\ncommand = \"x\"\n[mcp_servers.other]\ncommand = \"y\"\n",
        )
        .unwrap();
        fs::create_dir_all(&env.antigravity_dir).unwrap();
        fs::write(
            env.antigravity_mcp_config(),
            r#"{"mcpServers": {"xolkit": {"command": "x"}, "other": {"command": "y"}}}"#,
        )
        .unwrap();
        for provider in Provider::ALL {
            remove_entry_from_config(&env, provider).unwrap();
            assert!(!has_entry(&env, provider).unwrap());
        }
        assert!(
            fs::read_to_string(env.claude_json())
                .unwrap()
                .contains("other")
        );
        assert!(
            fs::read_to_string(env.codex_config())
                .unwrap()
                .contains("other")
        );
        assert!(
            fs::read_to_string(env.antigravity_mcp_config())
                .unwrap()
                .contains("other")
        );
    }

    #[test]
    fn registers_through_the_agents_tools() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let exe = Path::new("/usr/bin/xolkit");
        let line = |a: &Action| match a {
            Action::Run { provider, args, .. } | Action::Undo { provider, args, .. } => {
                format!("{} {}", provider.cli(), args.join(" "))
            }
            other => format!("{other:?}"),
        };
        let claude_target = Target::builtin(&env, Provider::Claude);
        let claude: Vec<_> = register(&env, &claude_target, exe)
            .iter()
            .map(line)
            .collect();
        assert_eq!(
            claude,
            ["claude mcp add --scope user xolkit -- /usr/bin/xolkit mcp --agent claude"]
        );
        let custom = CustomAgent {
            id: "claude-two".into(),
            name: "Claude (two)".into(),
            provider: Provider::Claude,
            dir: PathBuf::from("~/.claude-two"),
        };
        let custom_target = Target::custom(&env, &custom);
        let custom_steps = register(&env, &custom_target, exe);
        assert_eq!(
            custom_steps,
            [Action::Run {
                provider: Provider::Claude,
                args: [
                    "mcp",
                    "add",
                    "--scope",
                    "user",
                    "xolkit",
                    "--",
                    "/usr/bin/xolkit",
                    "mcp",
                    "--agent",
                    "claude-two",
                ]
                .map(String::from)
                .to_vec(),
                dir: Some(home.path().join(".claude-two")),
            }]
        );
        fs::create_dir_all(&env.codex_dir).unwrap();
        fs::write(
            env.codex_config(),
            "[mcp_servers.xolkit]\ncommand = \"/old\"\n",
        )
        .unwrap();
        let codex_target = Target::builtin(&env, Provider::Codex);
        let codex: Vec<_> = register(&env, &codex_target, exe)
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
        fs::create_dir_all(router_dir(&env, Provider::Claude)).unwrap();
        fs::write(
            router_dir(&env, Provider::Claude).join("SKILL.md"),
            format!("---\nname: xolkit\n---\n{ROUTER_MARK}\n"),
        )
        .unwrap();
        fs::write(
            env.claude_json(),
            r#"{"mcpServers": {"xolkit": {"command": "x"}}}"#,
        )
        .unwrap();
        let mut lock = Lock::default();
        lock.plugins.insert(
            "hooks@m".into(),
            PluginPin {
                native: BTreeSet::from([Agent::Claude]),
                ..PluginPin::default()
            },
        );
        let settings = Settings {
            router: vec![Agent::Claude],
            ..Settings::default()
        };
        let claude = Target::builtin(&env, Provider::Claude);
        let codex = Target::builtin(&env, Provider::Codex);
        let actions = unregister(&env, &claude, &lock, &settings);
        assert_eq!(actions.len(), 6, "{actions:?}");
        let Action::SaveSettings { settings: saved } = &actions[1] else {
            panic!("{actions:?}");
        };
        assert!(saved.router.is_empty());
        assert_eq!(
            actions[2],
            Action::RemoveRouter {
                provider: Provider::Claude,
                dir: None,
            }
        );
        assert!(unregister(&env, &codex, &lock, &settings).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn custom_config_dirs_pass_env_vars_and_isolate_registration() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let mut env = Env::at(home.path());
        let bin = home.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let claude_cli = bin.join("claude");
        fs::write(
            &claude_cli,
            "#!/bin/sh\nprintf '%s|%s\\n' \"$CLAUDE_CONFIG_DIR\" \"$*\" >> claude.log\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&claude_cli, fs::Permissions::from_mode(0o755)).unwrap();
        let codex_cli = bin.join("codex");
        fs::write(
            &codex_cli,
            "#!/bin/sh\nprintf '%s|%s\\n' \"$CODEX_HOME\" \"$*\" >> codex.log\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&codex_cli, fs::Permissions::from_mode(0o755)).unwrap();
        env.path = Some(bin.into_os_string());

        let mut settings = Settings::default();
        let claude_ix = settings
            .add_custom_agent(&env, "Claude (work)", Provider::Claude, "~/.claude-work")
            .unwrap();
        let codex_ix = settings
            .add_custom_agent(&env, "Codex (work)", Provider::Codex, "~/.codex-work")
            .unwrap();
        let ag_ix = settings
            .add_custom_agent(
                &env,
                "Antigravity (work)",
                Provider::Antigravity,
                "~/.gemini/antigravity-work",
            )
            .unwrap();

        let exe = Path::new("/usr/bin/xolkit");
        let claude_target = Target::custom(&env, &settings.custom_agents[claude_ix]);
        let codex_target = Target::custom(&env, &settings.custom_agents[codex_ix]);
        let ag_target = Target::custom(&env, &settings.custom_agents[ag_ix]);

        crate::execute(&env, &register(&env, &claude_target, exe)).unwrap();
        crate::execute(&env, &register(&env, &codex_target, exe)).unwrap();
        crate::execute(&env, &register(&env, &ag_target, exe)).unwrap();

        let claude_log = fs::read_to_string(home.path().join("claude.log")).unwrap();
        assert!(
            claude_log.contains(&format!(
                "{}|mcp add --scope user xolkit -- /usr/bin/xolkit mcp --agent {}",
                home.path().join(".claude-work").display(),
                claude_target.agent.key()
            )),
            "{claude_log}"
        );
        let codex_log = fs::read_to_string(home.path().join("codex.log")).unwrap();
        assert!(
            codex_log.contains(&format!(
                "{}|mcp add xolkit -- /usr/bin/xolkit mcp --agent {}",
                home.path().join(".codex-work").display(),
                codex_target.agent.key()
            )),
            "{codex_log}"
        );

        // Antigravity custom directory was written directly, leaving default untouched.
        assert_eq!(registration(&env, &ag_target, exe), Registration::Current);
        assert_eq!(
            registration(&env, &Target::builtin(&env, Provider::Antigravity), exe),
            Registration::Missing
        );

        // Router skill and unregister stay scoped to the custom Antigravity directory.
        let ag_env = env.for_target(&ag_target);
        let router_body = format!("---\nname: xolkit\n---\n{ROUTER_MARK}\n");
        write_router(&ag_env, Provider::Antigravity, Some(&router_body)).unwrap();
        assert!(router_is_ours(&ag_env, Provider::Antigravity));
        assert!(!router_is_ours(&env, Provider::Antigravity));

        let unreg = unregister(&env, &ag_target, &Lock::default(), &settings);
        crate::execute(&env, &unreg).unwrap();
        assert_eq!(registration(&env, &ag_target, exe), Registration::Missing);
        assert!(!router_is_ours(&ag_env, Provider::Antigravity));
    }
}
