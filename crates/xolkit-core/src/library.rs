//! Everything in `~/.xolkit`, read in one go.

use std::collections::BTreeMap;

use anyhow::Result;

use crate::lock::Lock;
use crate::plugins::scan_plugins;
use crate::servers::{read_servers, valid_server_name};
use crate::skills::scan_skills;
use crate::{Action, Env, Kind, Plugin, Server, ServerConfig, Settings, Skill};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Library {
    pub skills: Vec<Skill>,
    pub plugins: Vec<Plugin>,
    /// The servers in `servers.toml`, by name.
    pub servers: BTreeMap<String, ServerConfig>,
    pub lock: Lock,
    pub settings: Settings,
}

impl Library {
    pub fn load(env: &Env) -> Result<Library> {
        Ok(Library {
            skills: scan_skills(&env.skills_dir()),
            plugins: scan_plugins(&env.plugins_dir()),
            servers: read_servers(&env.servers_file())?,
            lock: Lock::read(&env.lock_file())?,
            settings: Settings::load(&env.settings_file())?,
        })
    }

    pub fn skill(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    pub fn plugin(&self, id: &str) -> Option<&Plugin> {
        self.plugins.iter().find(|p| p.id == id)
    }

    /// Every server xolkit connects to: those in `servers.toml`, then those plugins bring, each
    /// under a name of its own (`<plugin>-<server>`, or the plugin's name when it has one).
    pub fn all_servers(&self) -> Vec<Server> {
        let mut servers: Vec<Server> = self
            .servers
            .iter()
            .map(|(name, config)| Server {
                name: name.clone(),
                config: config.clone(),
                plugin: None,
            })
            .collect();
        for plugin in &self.plugins {
            let single = plugin.parts.servers.len() == 1;
            for (key, config) in &plugin.parts.servers {
                let wanted = if single {
                    plugin.name.clone()
                } else {
                    format!("{}-{key}", plugin.name)
                };
                let name = unique_name(&wanted, &servers);
                servers.push(Server {
                    name,
                    config: config.clone(),
                    plugin: Some(plugin.id.clone()),
                });
            }
        }
        servers
    }
}

/// `wanted` made into a valid server name no server in `taken` has.
fn unique_name(wanted: &str, taken: &[Server]) -> String {
    let base: String = wanted
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(28)
        .collect();
    let base = if valid_server_name(&base) {
        base
    } else {
        "plugin".to_owned()
    };
    (1..)
        .map(|n| {
            if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            }
        })
        .find(|name| !taken.iter().any(|s| &s.name == name))
        .unwrap_or(base)
}

/// Deletes a skill from the library and every profile.
pub fn uninstall_skill(env: &Env, settings: &Settings, name: &str) -> Vec<Action> {
    let mut settings = settings.clone();
    settings.forget(Kind::Skill, name);
    vec![
        Action::Remove {
            path: env.skills_dir().join(name),
        },
        Action::PinSkill {
            name: name.to_owned(),
            pin: None,
        },
        Action::SaveSettings {
            settings: Box::new(settings),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::write_plugin;
    use crate::skills::write_skill;
    use crate::{Agent, execute};

    #[test]
    fn loads_everything_and_names_plugin_servers() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        write_skill(&env.skills_dir().join("s"), "s");
        write_plugin(&env.plugins_dir(), "p@m", false);
        let mut settings = Settings::default();
        settings.active_mut().set(Kind::Skill, "s", Agent::Claude, true);
        execute(
            &env,
            &[
                Action::SaveServer {
                    name: "p".into(),
                    config: Some(ServerConfig {
                        command: Some("x".into()),
                        ..ServerConfig::default()
                    }),
                },
                Action::SaveSettings {
                    settings: Box::new(settings.clone()),
                },
            ],
        )
        .unwrap();
        let library = Library::load(&env).unwrap();
        assert_eq!(library.skill("s").unwrap().description, "Does s.");
        assert!(library.plugin("p@m").is_some());
        let names: Vec<_> = library.all_servers().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["p", "p-2"]);

        execute(&env, &uninstall_skill(&env, &settings, "s")).unwrap();
        let library = Library::load(&env).unwrap();
        assert!(library.skills.is_empty());
        assert!(library.settings.active().skills.is_empty());
    }
}
