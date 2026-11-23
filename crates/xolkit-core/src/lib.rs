//! xolkit's library of skills, plugins and MCP servers, and the operations on it. The daemon
//! serves what is here to agents; the app shows it and plans changes.

mod catalogue;
mod config;
mod discover;
mod env;
mod library;
mod lock;
mod marketplace;
mod ops;
mod plugins;
mod profiles;
mod registration;
mod secrets;
mod servers;
mod settings;
mod share;
mod skills;
mod update;

pub use catalogue::{Catalogue, Item, catalogue, router_skill};
pub use discover::{
    FetchedSkill, SkillHit, SkillUpdate, add_local_skill, check_skill_update, clean_staging,
    fetch_skill, fetch_skill_at, fetch_update, gh_token, github_rate_limit, install_skill,
    search_skills, update_skill,
};
pub use env::Env;
pub use library::{Library, uninstall_skill};
pub use lock::{Lock, Pin, PluginPin};
pub use marketplace::{
    FetchedPlugin, Offer, PluginSource, PluginUpdate, check_plugin_update, fetch_offers,
    fetch_plugin, fetch_plugin_at, fetch_plugin_update, install_plugin, native_changes,
    native_install, native_steps, native_toggle, uninstall_plugin, update_plugin,
    valid_marketplace,
};
pub use ops::{Action, execute};
pub use plugins::{Command, Parts, Plugin};
pub use profiles::{Kind, Profile, States};
pub use registration::{
    Registration, register, registration, router_is_ours, unregister, write_router,
};
pub use secrets::{GITHUB_TOKEN, get_secret, set_secret};
pub use servers::{
    Server, ServerConfig, XOLKIT, agent_servers, oauth_key, read_servers, secret_key,
    valid_server_name,
};
pub use settings::{DEFAULT_PROFILE, GitHubAccess, Settings};
pub use share::{
    Downloads, Shared, SharedItem, SharedServer, decode_code, encode_code, export_file,
    fetch_import, import_steps, read_export,
};
pub use skills::{Skill, strip_frontmatter};
pub use update::{
    Release, check_for_update, clean_old_exe, install_update, installed_by_package_manager,
};

use std::path::PathBuf;

/// An agent xolkit registers with.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
    Antigravity,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::Claude, Provider::Codex, Provider::Antigravity];

    pub fn label(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::Antigravity => "Antigravity",
        }
    }

    /// The product's full name.
    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "Claude Code",
            Provider::Codex => "Codex CLI",
            Provider::Antigravity => "Antigravity",
        }
    }

    /// Stable key saved in settings and passed to `xolkit mcp --agent`.
    pub fn key(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Antigravity => "antigravity",
        }
    }

    /// Name of the provider's command-line tool.
    pub fn cli(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Antigravity => "agy",
        }
    }

    /// Whether xolkit can install Claude-format native plugins into this provider.
    pub fn supports_native(self) -> bool {
        matches!(self, Provider::Claude | Provider::Codex)
    }

    /// Environment variable that points this provider's CLI at an alternative config directory.
    pub fn config_env_var(self) -> Option<&'static str> {
        match self {
            Provider::Claude => Some("CLAUDE_CONFIG_DIR"),
            Provider::Codex => Some("CODEX_HOME"),
            Provider::Antigravity => None,
        }
    }
}

/// An additional provider config directory configured in Settings, served as its own agent.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomAgent {
    /// Stable key in profiles, lock, router, and `xolkit mcp --agent <id>`.
    pub id: String,
    /// Display name shown in Settings, Profiles, and list columns.
    pub name: String,
    /// Which provider's config format and CLI this directory uses.
    pub provider: Provider,
    /// Path to the config directory; may start with `~/` (expanded against [`Env::home`]).
    pub dir: PathBuf,
}

/// A concrete provider configuration directory (built-in default or alternative) paired with the
/// [`Agent`] identity it serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub agent: Agent,
    pub provider: Provider,
    /// Full display name (e.g. "Claude Code" or "Claude (two)").
    pub name: String,
    /// Short column label (e.g. "Claude" or "Claude (two)").
    pub label: String,
    /// Resolved directory on disk.
    pub dir: PathBuf,
    /// `Some(dir)` for an alternative config directory; `None` for the built-in default.
    pub custom_dir: Option<PathBuf>,
}

impl Target {
    pub fn builtin(env: &Env, provider: Provider) -> Target {
        Target {
            agent: Agent::from(provider),
            provider,
            name: provider.name().to_owned(),
            label: provider.label().to_owned(),
            dir: env.provider_dir(provider).to_path_buf(),
            custom_dir: None,
        }
    }

    pub fn custom(env: &Env, custom: &CustomAgent) -> Target {
        let dir = env.expand_tilde(&custom.dir);
        Target {
            agent: Agent::Custom(custom.id.clone()),
            provider: custom.provider,
            name: custom.name.clone(),
            label: custom.name.clone(),
            custom_dir: Some(dir.clone()),
            dir,
        }
    }

    pub fn is_custom(&self) -> bool {
        self.custom_dir.is_some()
    }
}

/// Who a session serves: one of the agents xolkit registers with, an alternative provider config
/// configured in Settings, or any other MCP client. Each has its own switches for every skill,
/// plugin and server.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Agent {
    Claude,
    Codex,
    Antigravity,
    Other,
    /// An additional provider config directory, identified by its [`CustomAgent::id`].
    Custom(String),
}

/// Whether `id` is a valid key for a [`CustomAgent`]: 1..=32 ASCII lowercase letters, digits,
/// `-` or `_`, starting with a letter or digit, and not one of the built-in agent keys.
pub fn valid_custom_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && !matches!(id, "claude" | "codex" | "antigravity" | "other")
        && id.as_bytes()[0].is_ascii_lowercase()
            | id.as_bytes()[0].is_ascii_digit()
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Derives a unique, valid [`CustomAgent::id`] from `provider` and `name`.
pub fn slug_agent_id(provider: Provider, name: &str, taken: &[String]) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !slug.is_empty() && !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_matches('-').to_owned();
    let prefix = provider.key();
    let base = if slug.is_empty() || slug == prefix {
        format!("{prefix}-alt")
    } else if slug.starts_with(&format!("{prefix}-")) || slug.starts_with(&format!("{prefix}_")) {
        slug
    } else {
        format!("{prefix}-{slug}")
    };
    let base: String = base.chars().take(28).collect();
    let base = base.trim_end_matches(['-', '_']).to_owned();
    let base = if valid_custom_agent_id(&base) {
        base
    } else {
        format!("{prefix}-alt")
    };
    (1..)
        .map(|n| {
            if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            }
        })
        .find(|candidate| {
            valid_custom_agent_id(candidate) && !taken.iter().any(|existing| existing == candidate)
        })
        .unwrap_or(base)
}

impl Agent {
    pub const BUILTIN: [Agent; 4] = [
        Agent::Claude,
        Agent::Codex,
        Agent::Antigravity,
        Agent::Other,
    ];

    pub fn label(&self) -> &str {
        match self {
            Agent::Claude => "Claude",
            Agent::Codex => "Codex",
            Agent::Antigravity => "Antigravity",
            Agent::Other => "Other",
            Agent::Custom(id) => id.as_str(),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Agent::Claude => Provider::Claude.name(),
            Agent::Codex => Provider::Codex.name(),
            Agent::Antigravity => Provider::Antigravity.name(),
            Agent::Other => "Other MCP clients",
            Agent::Custom(id) => id.as_str(),
        }
    }

    /// The name `xolkit mcp --agent` takes.
    pub fn key(&self) -> &str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Antigravity => "antigravity",
            Agent::Other => "other",
            Agent::Custom(id) => id.as_str(),
        }
    }

    pub fn from_key(key: &str) -> Option<Agent> {
        match key {
            "claude" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            "antigravity" => Some(Agent::Antigravity),
            "other" => Some(Agent::Other),
            other if valid_custom_agent_id(other) => Some(Agent::Custom(other.to_owned())),
            _ => None,
        }
    }

    /// The agent an MCP client is, from the name it gives when it connects.
    pub fn from_client(name: &str) -> Agent {
        let name = name.to_lowercase();
        if name.contains("claude") {
            Agent::Claude
        } else if name.contains("codex") {
            Agent::Codex
        } else if name.contains("antigravity") {
            Agent::Antigravity
        } else {
            Agent::Other
        }
    }

    /// The built-in provider, if this is a built-in provider agent.
    pub fn provider(&self) -> Option<Provider> {
        match self {
            Agent::Claude => Some(Provider::Claude),
            Agent::Codex => Some(Provider::Codex),
            Agent::Antigravity => Some(Provider::Antigravity),
            Agent::Other | Agent::Custom(_) => None,
        }
    }
}

impl serde::Serialize for Agent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.key())
    }
}

impl<'de> serde::Deserialize<'de> for Agent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Agent::from_key(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid agent key: {s}")))
    }
}

impl From<Provider> for Agent {
    fn from(provider: Provider) -> Agent {
        match provider {
            Provider::Claude => Agent::Claude,
            Provider::Codex => Agent::Codex,
            Provider::Antigravity => Agent::Antigravity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_from_client_names() {
        assert_eq!(Agent::from_client("claude-code"), Agent::Claude);
        assert_eq!(Agent::from_client("codex-mcp-client"), Agent::Codex);
        assert_eq!(Agent::from_client("antigravity-client"), Agent::Antigravity);
        assert_eq!(Agent::from_client("cursor"), Agent::Other);
        assert_eq!(Agent::from_key("codex"), Some(Agent::Codex));
        assert_eq!(Agent::from_key("antigravity"), Some(Agent::Antigravity));
        assert_eq!(
            Agent::from_key("claude-two"),
            Some(Agent::Custom("claude-two".into()))
        );
        assert_eq!(Agent::from_key("Bad Key!"), None);
        assert_eq!(Agent::Other.provider(), None);
        assert_eq!(Agent::from(Provider::Claude).name(), "Claude Code");
        assert_eq!(Agent::from(Provider::Antigravity).name(), "Antigravity");
        assert_eq!(
            slug_agent_id(Provider::Claude, "Claude (two)", &[]),
            "claude-two"
        );
        assert_eq!(
            slug_agent_id(Provider::Claude, "Work", &["claude-work".into()]),
            "claude-work-2"
        );
        assert_eq!(
            slug_agent_id(Provider::Antigravity, "Personal", &[]),
            "antigravity-personal"
        );
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::path::Path;

    /// Links the folder `link` to `target`: a symlink, or a junction on Windows.
    pub fn link_dir(target: impl AsRef<Path>, link: impl AsRef<Path>) {
        crate::ops::link_dir(target.as_ref(), link.as_ref()).unwrap();
    }
}
