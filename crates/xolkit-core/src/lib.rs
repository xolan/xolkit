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
    FetchedPlugin, Offer, PluginSource, fetch_offers, fetch_plugin, fetch_plugin_at,
    install_plugin, native_install, native_steps, native_toggle, uninstall_plugin,
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
    Server, ServerConfig, XOLKIT, agent_servers, read_servers, secret_key, valid_server_name,
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

/// An agent xolkit registers with, saved by the name of its command-line tool.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Codex];

    pub fn label(self) -> &'static str {
        Agent::from(self).label()
    }

    /// The product's full name.
    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "Claude Code",
            Provider::Codex => "Codex CLI",
        }
    }

    /// Name of the provider's command-line tool.
    pub fn cli(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }
}

/// Who a session serves: one of the agents xolkit registers with, or any other MCP client.
/// Each has its own switches for every skill, plugin and server.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    Other,
}

impl Agent {
    pub const ALL: [Agent; 3] = [Agent::Claude, Agent::Codex, Agent::Other];

    pub fn label(self) -> &'static str {
        match self {
            Agent::Claude => "Claude",
            Agent::Codex => "Codex",
            Agent::Other => "Other",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Agent::Other => "Other MCP clients",
            agent => agent.provider().map_or("", Provider::name),
        }
    }

    /// The name `xolkit mcp --agent` takes.
    pub fn key(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Other => "other",
        }
    }

    pub fn from_key(key: &str) -> Option<Agent> {
        Agent::ALL.into_iter().find(|a| a.key() == key)
    }

    /// The agent an MCP client is, from the name it gives when it connects.
    pub fn from_client(name: &str) -> Agent {
        let name = name.to_lowercase();
        if name.contains("claude") {
            Agent::Claude
        } else if name.contains("codex") {
            Agent::Codex
        } else {
            Agent::Other
        }
    }

    pub fn provider(self) -> Option<Provider> {
        match self {
            Agent::Claude => Some(Provider::Claude),
            Agent::Codex => Some(Provider::Codex),
            Agent::Other => None,
        }
    }
}

impl From<Provider> for Agent {
    fn from(provider: Provider) -> Agent {
        match provider {
            Provider::Claude => Agent::Claude,
            Provider::Codex => Agent::Codex,
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
        assert_eq!(Agent::from_client("cursor"), Agent::Other);
        assert_eq!(Agent::from_key("codex"), Some(Agent::Codex));
        assert_eq!(Agent::from_key("gemini"), None);
        assert_eq!(Agent::Other.provider(), None);
        assert_eq!(Agent::from(Provider::Claude).name(), "Claude Code");
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
