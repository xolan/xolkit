//! `~/.xolkit/settings.json`: the profiles, the active one, and the app's choices. Secrets are
//! never here, so the file can live in a dotfiles repository.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::{read_json, write_json};
use crate::{Agent, Profile, Provider};

pub const DEFAULT_PROFILE: &str = "Default";
const MAX_NAME: usize = 40;
const DEFAULT_IDLE_MINUTES: u64 = 5;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// The default profile first, then the others in the order they were added.
    #[serde(default)]
    pub profiles: Vec<Profile>,
    /// The name of the profile in use.
    #[serde(default)]
    pub profile: String,
    /// Agents that get the router skill in their own skills folder.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub router: Vec<Provider>,
    /// Plugin marketplaces to browse, as GitHub `owner/repo`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marketplaces: Vec<String>,
    /// Minutes the daemon waits with no sessions and no app before it exits.
    #[serde(default = "default_idle")]
    pub idle_minutes: u64,
    /// Where GitHub downloads get a token from.
    #[serde(default)]
    pub github: GitHubAccess,
    /// The app's own choices, such as the theme, kept as they are.
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

fn default_idle() -> u64 {
    DEFAULT_IDLE_MINUTES
}

impl Default for Settings {
    fn default() -> Settings {
        Settings::from_json(Value::Object(Map::new()))
    }
}

/// Where GitHub downloads get a token from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitHubAccess {
    /// `gh auth token`.
    #[default]
    Gh,
    /// The token saved in the OS keyring.
    Token,
    /// No token: 60 requests an hour.
    None,
}

impl GitHubAccess {
    /// In display order; the token form sits under the last one.
    pub const ALL: [GitHubAccess; 3] = [GitHubAccess::Gh, GitHubAccess::None, GitHubAccess::Token];

    pub fn label(self) -> &'static str {
        match self {
            GitHubAccess::Gh => "GitHub CLI (gh)",
            GitHubAccess::Token => "Personal access token",
            GitHubAccess::None => "No token",
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Result<Settings> {
        Ok(Settings::from_json(read_json(path)?))
    }

    /// Reads settings, putting the default profile first and making sure the active one exists.
    /// Anything unreadable falls back to its default rather than failing the whole file.
    pub fn from_json(mut json: Value) -> Settings {
        if !json.is_object() {
            json = Value::Object(Map::new());
        }
        let profiles: Vec<Profile> = json["profiles"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| serde_json::from_value::<Profile>(p.clone()).ok())
            .filter(|p| !p.name.trim().is_empty())
            .collect();
        json["profiles"] = Value::Array(Vec::new());
        let mut settings: Settings = serde_json::from_value(json.clone()).unwrap_or_else(|_| {
            let other = json.as_object().cloned().unwrap_or_default();
            Settings {
                profiles: Vec::new(),
                profile: String::new(),
                router: Vec::new(),
                marketplaces: Vec::new(),
                idle_minutes: DEFAULT_IDLE_MINUTES,
                github: GitHubAccess::default(),
                other,
            }
        });
        settings.profiles = profiles;
        settings.normalize();
        settings
    }

    fn normalize(&mut self) {
        match self.profiles.iter().position(|p| p.name == DEFAULT_PROFILE) {
            Some(ix) => {
                let default = self.profiles.remove(ix);
                self.profiles.insert(0, default);
            }
            None => self
                .profiles
                .insert(0, Profile::new(DEFAULT_PROFILE, Vec::new())),
        }
        for profile in &mut self.profiles {
            profile.agents = Agent::ALL
                .into_iter()
                .filter(|a| profile.agents.contains(a))
                .collect();
        }
        if self.position(&self.profile).is_none() {
            self.profile = DEFAULT_PROFILE.to_owned();
        }
        self.idle_minutes = self.idle_minutes.max(1);
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_json(path, &serde_json::to_value(self)?).context("could not save the settings")
    }

    pub fn active_index(&self) -> usize {
        self.position(&self.profile).unwrap_or(0)
    }

    pub fn active(&self) -> &Profile {
        &self.profiles[self.active_index()]
    }

    pub fn active_mut(&mut self) -> &mut Profile {
        let ix = self.active_index();
        &mut self.profiles[ix]
    }

    pub fn position(&self, name: &str) -> Option<usize> {
        self.profiles.iter().position(|p| p.name == name)
    }

    /// A string the app saved, such as the theme.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.other.get(key).and_then(Value::as_str)
    }

    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        self.other.insert(key.to_owned(), value.into());
    }

    /// The trimmed name, if no other profile than the one at `renaming` has it.
    pub fn check_name(&self, name: &str, renaming: Option<usize>) -> Result<String> {
        let name = name.trim();
        ensure!(!name.is_empty(), "Give the profile a name.");
        ensure!(
            name.chars().count() <= MAX_NAME,
            "Keep the name to {MAX_NAME} characters."
        );
        let taken = self
            .profiles
            .iter()
            .position(|p| p.name.to_lowercase() == name.to_lowercase());
        if let Some(ix) = taken
            && Some(ix) != renaming
        {
            bail!(
                "There is a profile named “{}” already.",
                self.profiles[ix].name
            );
        }
        Ok(name.to_owned())
    }

    /// A name like `name` that no profile has yet: `name`, else `name 2`, `name 3`, …
    pub fn free_name(&self, name: &str) -> String {
        let base: String = name.trim().chars().take(MAX_NAME - 3).collect();
        let base = if base.is_empty() {
            "Imported".to_owned()
        } else {
            base
        };
        (1..)
            .map(|n| {
                if n == 1 {
                    base.clone()
                } else {
                    format!("{base} {n}")
                }
            })
            .find(|candidate| self.check_name(candidate, None).is_ok())
            .unwrap_or(base)
    }

    /// Adds a copy of the active profile and returns where it is.
    pub fn add(&mut self, name: &str) -> Result<usize> {
        let name = self.check_name(name, None)?;
        let profile = Profile {
            name,
            ..self.active().clone()
        };
        self.profiles.push(profile);
        Ok(self.profiles.len() - 1)
    }

    pub fn rename(&mut self, ix: usize, name: &str) -> Result<()> {
        ensure!(ix != 0, "The default profile can't be renamed.");
        let name = self.check_name(name, Some(ix))?;
        if self.active_index() == ix {
            self.profile = name.clone();
        }
        self.profiles[ix].name = name;
        Ok(())
    }

    pub fn remove(&mut self, ix: usize) -> Result<()> {
        ensure!(ix != 0, "The default profile can't be removed.");
        ensure!(
            ix != self.active_index(),
            "Switch to another profile before removing this one."
        );
        self.profiles.remove(ix);
        Ok(())
    }

    /// Every agent some profile serves, in display order.
    pub fn all_agents(&self) -> Vec<Agent> {
        Agent::ALL
            .into_iter()
            .filter(|a| self.profiles.iter().any(|p| p.agents.contains(a)))
            .collect()
    }

    /// Drops an uninstalled item from every profile.
    pub fn forget(&mut self, kind: crate::Kind, name: &str) {
        for profile in &mut self.profiles {
            profile.forget(kind, name);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::Kind;

    #[test]
    fn a_missing_or_broken_file_gets_the_default_profile() {
        for json in [
            json!({}),
            json!(null),
            json!({"profiles": "nope", "idleMinutes": "x"}),
        ] {
            let settings = Settings::from_json(json);
            assert_eq!(settings.profiles.len(), 1);
            assert_eq!(settings.active().name, DEFAULT_PROFILE);
            assert!(settings.active().agents.is_empty());
        }
    }

    #[test]
    fn loads_the_default_first_and_the_saved_active_one() {
        let settings = Settings::from_json(json!({
            "profiles": [
                {"name": "Work", "agents": ["other", "claude"]},
                {"name": "Default", "agents": ["codex"]},
                {"name": "  "}
            ],
            "profile": "Work",
            "theme": "xolkit Dark",
            "github": "token"
        }));
        let names: Vec<_> = settings.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Default", "Work"]);
        assert_eq!(settings.active().name, "Work");
        assert_eq!(settings.active().agents, [Agent::Claude, Agent::Other]);
        assert_eq!(settings.get("theme"), Some("xolkit Dark"));
        assert_eq!(settings.github, GitHubAccess::Token);
        assert_eq!(settings.idle_minutes, DEFAULT_IDLE_MINUTES);
        let saved = serde_json::to_value(&settings).unwrap();
        assert_eq!(saved["theme"], "xolkit Dark");
        assert_eq!(saved["profile"], "Work");
        assert_eq!(Settings::from_json(saved), settings);
    }

    #[test]
    fn adds_renames_and_removes_profiles() {
        let mut settings = Settings::default();
        settings
            .active_mut()
            .set(Kind::Skill, "s", Agent::Claude, true);
        let ix = settings.add(" Work ").unwrap();
        assert_eq!(settings.profiles[ix].name, "Work");
        assert!(settings.profiles[ix].switched_on(Kind::Skill, "s", Agent::Claude));
        assert!(settings.add("work").is_err());
        assert!(settings.rename(0, "x").is_err());
        settings.profile = "Work".into();
        settings.rename(ix, "Job").unwrap();
        assert_eq!(settings.profile, "Job");
        assert!(settings.remove(ix).is_err());
        settings.profile = DEFAULT_PROFILE.into();
        settings.remove(ix).unwrap();
        assert_eq!(settings.profiles.len(), 1);
        assert_eq!(settings.free_name("Default"), "Default 2");
        assert_eq!(settings.free_name("New"), "New");
    }
}
