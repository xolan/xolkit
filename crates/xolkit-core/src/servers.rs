//! The MCP servers xolkit connects to, kept in `~/.xolkit/servers.toml`:
//!
//! ```toml
//! [servers.github]
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-github"]
//! secrets = ["GITHUB_PERSONAL_ACCESS_TOKEN"]
//!
//! [servers.docs]
//! url = "https://example.com/mcp"
//! secrets = ["Authorization"]
//! ```
//!
//! A local server has a `command`; a remote one a `url`. Names in `secrets` are environment
//! variables (local) or headers (remote) whose values live in the OS keyring, not in the file.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{read_json, read_toml, write_atomic};
use crate::{Env, Provider};

/// The name xolkit registers itself under in each agent.
pub const XOLKIT: &str = "xolkit";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Environment variables or headers whose values are in the OS keyring.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
}

impl ServerConfig {
    pub fn is_remote(&self) -> bool {
        self.url.is_some()
    }

    /// `local` or `remote`, as the app shows it.
    pub fn kind(&self) -> &'static str {
        if self.is_remote() { "remote" } else { "local" }
    }

    /// The command line or URL, for display.
    pub fn summary(&self) -> String {
        match (&self.url, &self.command) {
            (Some(url), _) => url.clone(),
            (None, Some(command)) => std::iter::once(command.as_str())
                .chain(self.args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            (None, None) => String::new(),
        }
    }

    pub fn check(&self) -> Result<()> {
        match (&self.command, &self.url) {
            (Some(command), None) => ensure!(!command.trim().is_empty(), "the command is empty"),
            (None, Some(url)) => ensure!(
                url.starts_with("https://") || url.starts_with("http://"),
                "the URL must start with https:// or http://"
            ),
            (Some(_), Some(_)) => anyhow::bail!("a server has a command or a URL, not both"),
            (None, None) => anyhow::bail!("a server needs a command or a URL"),
        }
        Ok(())
    }
}

/// A server xolkit connects to: one from `servers.toml`, or one a plugin brings.
#[derive(Clone, Debug, PartialEq)]
pub struct Server {
    /// Unique across xolkit; the prefix of its tools' names.
    pub name: String,
    pub config: ServerConfig,
    /// The plugin it comes with, by id.
    pub plugin: Option<String>,
}

/// The keyring entry holding the value of `key` (an environment variable or header) for server
/// `name`.
pub fn secret_key(name: &str, key: &str) -> String {
    format!("server/{name}/{key}")
}

/// Letters, digits, `-` and `_`, up to 32 characters: it goes into tool names, which agents
/// limit to 64 characters.
pub fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name != XOLKIT
        && !name.contains("__")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Default, Serialize, Deserialize)]
struct ServersFile {
    #[serde(default)]
    servers: BTreeMap<String, ServerConfig>,
}

/// The servers in `servers.toml`, by name.
pub fn read_servers(path: &Path) -> Result<BTreeMap<String, ServerConfig>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
    };
    let file: ServersFile =
        toml::from_str(&text).with_context(|| format!("{} is not valid", path.display()))?;
    Ok(file.servers)
}

pub(crate) fn write_servers(path: &Path, servers: &BTreeMap<String, ServerConfig>) -> Result<()> {
    let file = ServersFile {
        servers: servers.clone(),
    };
    write_atomic(path, toml::to_string_pretty(&file)?.as_bytes())
}

/// MCP servers already set up in an agent's own config, user scope only, without xolkit itself:
/// candidates to copy into xolkit. Reads the files; never changes them.
pub fn agent_servers(env: &Env, provider: Provider) -> Result<BTreeMap<String, ServerConfig>> {
    let mut servers = BTreeMap::new();
    match provider {
        Provider::Claude => {
            let json = read_json(&env.claude_json())?;
            for (name, entry) in json["mcpServers"].as_object().into_iter().flatten() {
                if let Some(config) = from_json(entry, None) {
                    servers.insert(name.clone(), config);
                }
            }
        }
        Provider::Codex => {
            let doc = read_toml(&env.codex_config())?;
            let json: Value = toml::from_str(&doc.to_string()).unwrap_or_default();
            for (name, entry) in json["mcp_servers"].as_object().into_iter().flatten() {
                let mut config = from_json(entry, None);
                if let Some(config) = &mut config
                    && config.url.is_none()
                    && let Some(url) = entry["url"].as_str()
                {
                    config.url = Some(url.to_owned());
                }
                if let Some(config) = config {
                    servers.insert(name.clone(), config);
                }
            }
        }
    }
    servers.remove(XOLKIT);
    Ok(servers)
}

/// A server entry in the shape Claude Code's `.mcp.json` and settings use. `root` replaces
/// `${CLAUDE_PLUGIN_ROOT}`, for servers a plugin brings.
pub(crate) fn from_json(entry: &Value, root: Option<&Path>) -> Option<ServerConfig> {
    let expand = |text: &str| match root {
        Some(root) => text.replace("${CLAUDE_PLUGIN_ROOT}", &root.to_string_lossy()),
        None => text.to_owned(),
    };
    let strings = |value: &Value| -> BTreeMap<String, String> {
        value
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| Some((k.clone(), expand(v.as_str()?))))
            .collect()
    };
    let config = if let Some(url) = entry["url"].as_str() {
        ServerConfig {
            url: Some(expand(url)),
            headers: strings(&entry["headers"]),
            ..ServerConfig::default()
        }
    } else {
        ServerConfig {
            command: Some(expand(entry["command"].as_str()?)),
            args: entry["args"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| Some(expand(a.as_str()?)))
                .collect(),
            env: strings(&entry["env"]),
            cwd: entry["cwd"].as_str().map(|c| PathBuf::from(expand(c))),
            ..ServerConfig::default()
        }
    };
    Some(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn servers_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("servers.toml");
        assert!(read_servers(&path).unwrap().is_empty());
        let servers = BTreeMap::from([
            (
                "github".to_owned(),
                ServerConfig {
                    command: Some("npx".into()),
                    args: vec!["-y".into(), "server-github".into()],
                    secrets: vec!["GITHUB_TOKEN".into()],
                    ..ServerConfig::default()
                },
            ),
            (
                "docs".to_owned(),
                ServerConfig {
                    url: Some("https://example.com/mcp".into()),
                    headers: BTreeMap::from([("X-Team".into(), "core".into())]),
                    ..ServerConfig::default()
                },
            ),
        ]);
        write_servers(&path, &servers).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[servers.github]"), "{text}");
        assert_eq!(read_servers(&path).unwrap(), servers);
        assert_eq!(servers["github"].summary(), "npx -y server-github");
        assert_eq!(servers["docs"].kind(), "remote");
    }

    #[test]
    fn checks_names_and_configs() {
        assert!(valid_server_name("git-hub_2"));
        for bad in ["", "xolkit", "a b", "a__b", &"x".repeat(33)] {
            assert!(!valid_server_name(bad), "{bad}");
        }
        assert!(ServerConfig::default().check().is_err());
        let ftp = ServerConfig {
            url: Some("ftp://x".into()),
            ..ServerConfig::default()
        };
        assert!(ftp.check().is_err());
    }

    #[test]
    fn reads_plugin_and_agent_entries() {
        let root = Path::new("/p");
        let local = from_json(
            &json!({"command": "${CLAUDE_PLUGIN_ROOT}/bin/s", "args": ["--x"], "env": {"A": "1"}}),
            Some(root),
        )
        .unwrap();
        assert_eq!(local.command.as_deref(), Some("/p/bin/s"));
        assert_eq!(local.env["A"], "1");
        let remote = from_json(
            &json!({"type": "http", "url": "https://x/mcp", "headers": {"H": "v"}}),
            None,
        )
        .unwrap();
        assert_eq!(remote.url.as_deref(), Some("https://x/mcp"));
        assert!(from_json(&json!({"type": "stdio"}), None).is_none());
    }

    #[test]
    fn copies_servers_from_agent_configs() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        fs::write(
            env.claude_json(),
            r#"{"mcpServers": {"xolkit": {"command": "xolkit"}, "fs": {"command": "fs-server"}}}"#,
        )
        .unwrap();
        fs::create_dir_all(&env.codex_dir).unwrap();
        fs::write(
            env.codex_config(),
            "[mcp_servers.docs]\nurl = \"https://x/mcp\"\n[mcp_servers.xolkit]\ncommand = \"x\"\n",
        )
        .unwrap();
        let claude = agent_servers(&env, Provider::Claude).unwrap();
        assert_eq!(claude.keys().collect::<Vec<_>>(), ["fs"]);
        let codex = agent_servers(&env, Provider::Codex).unwrap();
        assert_eq!(codex["docs"].url.as_deref(), Some("https://x/mcp"));
        assert_eq!(codex.len(), 1);
    }
}
