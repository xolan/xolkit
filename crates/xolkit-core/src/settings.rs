//! `~/.xolkit/settings.json`: the profiles, the active one, and the app's choices. Secrets are
//! never here, so the file can live in a dotfiles repository.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::{read_json, write_json};
use crate::{
    Agent, CustomAgent, Env, Profile, Provider, Target, slug_agent_id, valid_custom_agent_id,
};

pub const DEFAULT_PROFILE: &str = "Default";
const MAX_NAME: usize = 40;
const MAX_CUSTOM_AGENT_NAME: usize = 32;
const DEFAULT_IDLE_MINUTES: u64 = 5;
const DEFAULT_SESSION_IDLE_MINUTES: u64 = 10;

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
    pub router: Vec<Agent>,
    /// Additional provider config directories, each served as its own agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_agents: Vec<CustomAgent>,
    /// Plugin marketplaces to browse, as GitHub `owner/repo`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marketplaces: Vec<String>,
    /// Minutes the daemon waits with no sessions and no app before it exits.
    #[serde(default = "default_idle")]
    pub idle_minutes: u64,
    /// Minutes an agent session may go without a message before its shim lets go of the daemon;
    /// it connects again with the agent's next message. `0` keeps sessions connected.
    #[serde(default = "default_session_idle")]
    pub session_idle_minutes: u64,
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

fn default_session_idle() -> u64 {
    DEFAULT_SESSION_IDLE_MINUTES
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
        let custom_agents: Vec<CustomAgent> = json["customAgents"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| serde_json::from_value::<CustomAgent>(a.clone()).ok())
            .collect();
        json["profiles"] = Value::Array(Vec::new());
        json["customAgents"] = Value::Array(Vec::new());
        let mut settings: Settings = serde_json::from_value(json.clone()).unwrap_or_else(|_| {
            let other = json.as_object().cloned().unwrap_or_default();
            Settings {
                profiles: Vec::new(),
                profile: String::new(),
                router: Vec::new(),
                custom_agents: Vec::new(),
                marketplaces: Vec::new(),
                idle_minutes: DEFAULT_IDLE_MINUTES,
                session_idle_minutes: DEFAULT_SESSION_IDLE_MINUTES,
                github: GitHubAccess::default(),
                other,
            }
        });
        settings.profiles = profiles;
        settings.custom_agents = custom_agents;
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
        let mut seen_ids = HashSet::new();
        self.custom_agents.retain(|a| {
            valid_custom_agent_id(&a.id)
                && !a.name.trim().is_empty()
                && !a.dir.as_os_str().is_empty()
                && seen_ids.insert(a.id.clone())
        });
        let known = self.known_agents();
        self.router
            .retain(|a| *a != Agent::Other && known.contains(a));
        for profile in &mut self.profiles {
            profile.agents = known
                .iter()
                .filter(|a| profile.agents.contains(a))
                .cloned()
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

    /// Every agent known to this configuration, in display order: built-in providers, then
    /// configured alternative config paths, then `Other`.
    pub fn known_agents(&self) -> Vec<Agent> {
        let mut agents: Vec<Agent> = Provider::ALL.into_iter().map(Agent::from).collect();
        agents.extend(
            self.custom_agents
                .iter()
                .map(|a| Agent::Custom(a.id.clone())),
        );
        agents.push(Agent::Other);
        agents
    }

    /// Every provider target: the built-in providers, then any configured alternative paths.
    pub fn targets(&self, env: &Env) -> Vec<Target> {
        let mut targets: Vec<Target> = Provider::ALL
            .into_iter()
            .map(|p| Target::builtin(env, p))
            .collect();
        targets.extend(self.custom_agents.iter().map(|c| Target::custom(env, c)));
        targets
    }

    /// Every target that supports Claude-format native plugins.
    pub fn native_targets(&self, env: &Env) -> Vec<Target> {
        self.targets(env)
            .into_iter()
            .filter(|t| t.provider.supports_native())
            .collect()
    }

    /// The provider target for `agent`, if it is a built-in provider or configured alternative.
    pub fn target(&self, env: &Env, agent: &Agent) -> Option<Target> {
        if let Some(provider) = agent.provider() {
            return Some(Target::builtin(env, provider));
        }
        if let Agent::Custom(id) = agent {
            return self
                .custom_agents
                .iter()
                .find(|c| c.id == *id)
                .map(|c| Target::custom(env, c));
        }
        None
    }

    /// Short display label for `agent` (e.g. "Claude", "Antigravity", "Claude (two)", "Other").
    pub fn agent_label(&self, agent: &Agent) -> String {
        if let Agent::Custom(id) = agent
            && let Some(custom) = self.custom_agents.iter().find(|c| c.id == *id)
        {
            return custom.name.clone();
        }
        agent.label().to_owned()
    }

    /// Full display name for `agent` (e.g. "Claude Code", "Claude (two)", "Other MCP clients").
    pub fn agent_name(&self, agent: &Agent) -> String {
        if let Agent::Custom(id) = agent
            && let Some(custom) = self.custom_agents.iter().find(|c| c.id == *id)
        {
            return custom.name.clone();
        }
        agent.name().to_owned()
    }

    /// Checks a name and config folder for an alternative provider config path.
    pub fn check_custom_agent(
        &self,
        env: &Env,
        name: &str,
        provider: Provider,
        dir: &str,
        editing: Option<usize>,
    ) -> Result<(String, PathBuf)> {
        let name = name.trim();
        ensure!(!name.is_empty(), "Give this config path a name.");
        ensure!(
            !name.chars().any(char::is_control),
            "Keep the name to one line."
        );
        ensure!(
            name.chars().count() <= MAX_CUSTOM_AGENT_NAME,
            "Keep the name to {MAX_CUSTOM_AGENT_NAME} characters."
        );
        let lower = name.to_lowercase();
        let reserved = Agent::BUILTIN.iter().any(|a| {
            a.label().to_lowercase() == lower
                || a.name().to_lowercase() == lower
                || a.key() == lower
        });
        ensure!(!reserved, "“{name}” is already used by a built-in agent.");
        if let Some(ix) = self
            .custom_agents
            .iter()
            .position(|a| a.name.to_lowercase() == lower)
            && Some(ix) != editing
        {
            bail!(
                "There is an alternative config named “{}” already.",
                self.custom_agents[ix].name
            );
        }

        let dir_str = dir.trim();
        ensure!(!dir_str.is_empty(), "Enter a config folder path.");
        ensure!(
            !dir_str.chars().any(char::is_control),
            "Keep the folder path to one line."
        );
        let raw_dir = PathBuf::from(dir_str);
        let starts_with_tilde =
            dir_str == "~" || dir_str.starts_with("~/") || dir_str.starts_with("~\\");
        ensure!(
            starts_with_tilde || raw_dir.is_absolute(),
            "Use an absolute path, or one starting with ~/."
        );
        let expanded = env.expand_tilde(&raw_dir);
        let default_dir = env.expand_tilde(env.provider_dir(provider));
        ensure!(
            expanded != default_dir,
            "{} is already the default folder for {}.",
            env.tilde(&default_dir),
            provider.name()
        );
        if let Some(ix) = self.custom_agents.iter().position(|a| {
            a.provider == provider && env.expand_tilde(&a.dir) == expanded
        }) && Some(ix) != editing
        {
            bail!(
                "“{}” already uses {}.",
                self.custom_agents[ix].name,
                env.tilde(&expanded)
            );
        }
        // Keep `~/…` as typed when inside home so settings.json stays portable.
        let stored_dir: PathBuf = raw_dir.components().collect();
        Ok((name.to_owned(), stored_dir))
    }

    /// Adds an alternative provider config path and returns its index in `custom_agents`.
    pub fn add_custom_agent(
        &mut self,
        env: &Env,
        name: &str,
        provider: Provider,
        dir: &str,
    ) -> Result<usize> {
        let (name, dir) = self.check_custom_agent(env, name, provider, dir, None)?;
        let taken: Vec<String> = self.custom_agents.iter().map(|a| a.id.clone()).collect();
        let id = slug_agent_id(provider, &name, &taken);
        self.custom_agents.push(CustomAgent {
            id,
            name,
            provider,
            dir,
        });
        Ok(self.custom_agents.len() - 1)
    }

    /// Updates an existing alternative provider config path in place, keeping its `id`.
    pub fn update_custom_agent(
        &mut self,
        env: &Env,
        ix: usize,
        name: &str,
        provider: Provider,
        dir: &str,
    ) -> Result<()> {
        ensure!(ix < self.custom_agents.len(), "config path not found");
        let (name, dir) = self.check_custom_agent(env, name, provider, dir, Some(ix))?;
        self.custom_agents[ix].name = name;
        self.custom_agents[ix].provider = provider;
        self.custom_agents[ix].dir = dir;
        Ok(())
    }

    /// Removes an alternative provider config path, dropping it from `router` and every profile.
    pub fn remove_custom_agent(&mut self, ix: usize) -> Option<CustomAgent> {
        if ix >= self.custom_agents.len() {
            return None;
        }
        let removed = self.custom_agents.remove(ix);
        let agent = Agent::Custom(removed.id.clone());
        self.router.retain(|a| a != &agent);
        for profile in &mut self.profiles {
            profile.forget_agent(&agent);
        }
        Some(removed)
    }

    /// The trimmed name, if no other profile than the one at `renaming` has it.
    pub fn check_name(&self, name: &str, renaming: Option<usize>) -> Result<String> {
        let name = name.trim();
        ensure!(!name.is_empty(), "Give the profile a name.");
        ensure!(
            !name.chars().any(char::is_control),
            "Keep the name to one line."
        );
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
        // Names come from shared profiles too; no control characters, so none can fake a line.
        let base: String = name
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>()
            .trim()
            .chars()
            .take(MAX_NAME - 3)
            .collect();
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
        self.known_agents()
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
                {"name": "Work", "agents": ["other", "claude-two", "claude", "antigravity"]},
                {"name": "Default", "agents": ["codex"]},
                {"name": "  "}
            ],
            "customAgents": [
                {"id": "claude-two", "name": "Claude (two)", "provider": "claude", "dir": "~/.claude-two"}
            ],
            "router": ["claude", "claude-two", "antigravity"],
            "profile": "Work",
            "theme": "xolkit Dark",
            "github": "token"
        }));
        let names: Vec<_> = settings.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Default", "Work"]);
        assert_eq!(settings.active().name, "Work");
        assert_eq!(
            settings.active().agents,
            [
                Agent::Claude,
                Agent::Antigravity,
                Agent::Custom("claude-two".into()),
                Agent::Other
            ]
        );
        assert_eq!(settings.agent_label(&Agent::Custom("claude-two".into())), "Claude (two)");
        assert_eq!(settings.router.len(), 3);
        assert_eq!(settings.get("theme"), Some("xolkit Dark"));
        assert_eq!(settings.github, GitHubAccess::Token);
        assert_eq!(settings.idle_minutes, DEFAULT_IDLE_MINUTES);
        assert_eq!(settings.session_idle_minutes, DEFAULT_SESSION_IDLE_MINUTES);
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
        assert!(settings.profiles[ix].switched_on(Kind::Skill, "s", &Agent::Claude));
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

    #[test]
    fn validates_and_manages_custom_agents() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let mut settings = Settings::default();
        // Cannot replace the default directory or use a built-in name or relative path.
        assert!(
            settings
                .add_custom_agent(&env, "Claude (two)", Provider::Claude, "~/.claude")
                .is_err()
        );
        assert!(
            settings
                .add_custom_agent(&env, "Claude", Provider::Claude, "~/.claude-two")
                .is_err()
        );
        assert!(
            settings
                .add_custom_agent(&env, "Work", Provider::Claude, "relative/dir")
                .is_err()
        );
        let ix = settings
            .add_custom_agent(&env, "Claude (two)", Provider::Claude, "~/.claude-two")
            .unwrap();
        assert_eq!(settings.custom_agents[ix].id, "claude-two");
        // Duplicate folder for same provider is rejected.
        assert!(
            settings
                .add_custom_agent(
                    &env,
                    "Another",
                    Provider::Claude,
                    &home.path().join(".claude-two").display().to_string()
                )
                .is_err()
        );
        let agent = Agent::Custom("claude-two".into());
        settings.router.push(agent.clone());
        settings.active_mut().set_agent(agent.clone(), true);
        settings.active_mut().set(Kind::Skill, "s", agent.clone(), true);
        settings
            .update_custom_agent(&env, ix, "Claude Work", Provider::Claude, "~/.claude-work")
            .unwrap();
        assert_eq!(settings.custom_agents[ix].id, "claude-two");
        assert_eq!(settings.custom_agents[ix].name, "Claude Work");
        settings.remove_custom_agent(ix).unwrap();
        assert!(settings.custom_agents.is_empty());
        assert!(settings.router.is_empty());
        assert!(!settings.active().serves(&agent));
        assert!(!settings.active().switched_on(Kind::Skill, "s", &agent));
    }
}
