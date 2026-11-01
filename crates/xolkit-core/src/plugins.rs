//! Plugins in the library, unpacked under `~/.xolkit/plugins/<id>`, in Claude Code's plugin
//! layout. xolkit serves the parts MCP can carry itself: skills, commands (as prompts) and MCP
//! servers. Hooks, subagents, output styles and LSP servers only work from an agent's own
//! folders, so a plugin with any of them is installed natively as well.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::read_json;
use crate::servers::from_json;
use crate::skills::{frontmatter, scan_skills, strip_frontmatter};
use crate::{ServerConfig, Skill};

#[derive(Clone, Debug, PartialEq)]
pub struct Plugin {
    /// `name@marketplace`.
    pub id: String,
    pub name: String,
    pub marketplace: String,
    pub version: String,
    pub description: String,
    pub dir: PathBuf,
    pub parts: Parts,
}

/// What a plugin brings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Parts {
    pub skills: Vec<Skill>,
    /// Slash commands, served as prompts.
    pub commands: Vec<Command>,
    /// MCP servers, by the name the plugin gives them.
    pub servers: BTreeMap<String, ServerConfig>,
    /// Parts only an agent can run, such as "hooks": a plugin with any needs a native install.
    pub native: Vec<&'static str>,
}

/// A plugin's slash command: a Markdown prompt, `$ARGUMENTS` replaced by what the user types.
#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

impl Command {
    /// The prompt text, with `$ARGUMENTS` replaced.
    pub fn render(&self, arguments: &str) -> String {
        let text = fs::read_to_string(&self.path).unwrap_or_default();
        strip_frontmatter(&text).replace("$ARGUMENTS", arguments)
    }
}

impl Plugin {
    /// Reads the plugin unpacked in `dir`, whose folder name is its id. `None` when the folder
    /// name isn't an id.
    pub fn read(dir: &Path) -> Option<Plugin> {
        Plugin::read_as(dir, dir.file_name()?.to_str()?)
    }

    /// Reads the plugin in `dir` as plugin `id`, wherever it is unpacked.
    pub fn read_as(dir: &Path, id: &str) -> Option<Plugin> {
        let id = id.to_owned();
        let (name, marketplace) = id.split_once('@')?;
        let manifest =
            read_json(&dir.join(".claude-plugin").join("plugin.json")).unwrap_or(Value::Null);
        Some(Plugin {
            name: name.to_owned(),
            marketplace: marketplace.to_owned(),
            version: manifest["version"].as_str().unwrap_or_default().to_owned(),
            description: manifest["description"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            parts: read_parts(dir, &manifest),
            dir: dir.to_path_buf(),
            id,
        })
    }

    pub fn needs_native(&self) -> bool {
        !self.parts.native.is_empty()
    }
}

/// The plugins in `root`, sorted by id.
pub(crate) fn scan_plugins(root: &Path) -> Vec<Plugin> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut plugins: Vec<Plugin> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| Plugin::read(&e.path()))
        .collect();
    plugins.sort_by(|a, b| a.id.cmp(&b.id));
    plugins
}

/// Paths a manifest field lists, relative to the plugin, as a string or a list of strings.
fn manifest_paths(dir: &Path, value: &Value) -> Vec<PathBuf> {
    let list: Vec<&str> = match value {
        Value::String(s) => vec![s.as_str()],
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    list.into_iter()
        .map(|p| dir.join(p.trim_start_matches("./")))
        .collect()
}

fn read_parts(dir: &Path, manifest: &Value) -> Parts {
    let mut parts = Parts {
        skills: scan_skills(&dir.join("skills")),
        ..Parts::default()
    };
    for extra in manifest_paths(dir, &manifest["skills"]) {
        parts.skills.extend(scan_skills(&extra));
    }

    let mut command_dirs = vec![dir.join("commands")];
    command_dirs.extend(manifest_paths(dir, &manifest["commands"]));
    for path in command_dirs {
        parts.commands.extend(read_commands(&path));
    }
    parts.commands.sort_by(|a, b| a.name.cmp(&b.name));
    parts.commands.dedup_by(|a, b| a.name == b.name);

    // `.mcp.json`, and whatever the manifest adds: servers inline, or files of them.
    let mut servers = serde_json::Map::new();
    let mut add = |json: Value| {
        let json = json.get("mcpServers").cloned().unwrap_or(json);
        if let Value::Object(more) = json {
            servers.extend(more);
        }
    };
    add(read_json(&dir.join(".mcp.json")).unwrap_or(Value::Null));
    match &manifest["mcpServers"] {
        Value::Object(_) => add(manifest["mcpServers"].clone()),
        other => {
            for path in manifest_paths(dir, other) {
                add(read_json(&path).unwrap_or(Value::Null));
            }
        }
    }
    for (name, entry) in &servers {
        if let Some(config) = from_json(entry, Some(dir)) {
            parts.servers.insert(name.clone(), config);
        }
    }

    let non_empty =
        |path: PathBuf| fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_some());
    if non_empty(dir.join("agents")) || !manifest["agents"].is_null() {
        parts.native.push("subagents");
    }
    if dir.join("hooks").join("hooks.json").is_file() || !manifest["hooks"].is_null() {
        parts.native.push("hooks");
    }
    if non_empty(dir.join("output-styles")) || !manifest["outputStyles"].is_null() {
        parts.native.push("output styles");
    }
    if dir.join(".lsp.json").is_file() || !manifest["lspServers"].is_null() {
        parts.native.push("LSP servers");
    }
    parts
}

/// The `.md` files in `dir` and its subfolders; a subfolder becomes part of the name
/// (`git/commit.md` is `git:commit`), as in Claude Code.
fn read_commands(dir: &Path) -> Vec<Command> {
    let mut commands = Vec::new();
    collect_commands(dir, "", &mut commands);
    commands
}

fn collect_commands(dir: &Path, prefix: &str, commands: &mut Vec<Command>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        let name = if prefix.is_empty() {
            stem.clone()
        } else {
            format!("{prefix}:{stem}")
        };
        if path.is_dir() {
            collect_commands(&path, &name, commands);
        } else if path.extension().is_some_and(|e| e == "md") {
            let text = fs::read_to_string(&path).unwrap_or_default();
            let description = frontmatter(&text).description.unwrap_or_else(|| {
                strip_frontmatter(&text)
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or_default()
                    .trim_start_matches('#')
                    .trim()
                    .to_owned()
            });
            commands.push(Command {
                name,
                description,
                path,
            });
        }
    }
}

#[cfg(test)]
pub(crate) fn write_plugin(root: &Path, id: &str, native: bool) -> PathBuf {
    use crate::skills::write_skill;
    let dir = root.join(id);
    fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
    fs::write(
        dir.join(".claude-plugin/plugin.json"),
        r#"{"name": "p", "version": "1.2.0", "description": "A plugin."}"#,
    )
    .unwrap();
    write_skill(&dir.join("skills/helper"), "helper");
    fs::create_dir_all(dir.join("commands/git")).unwrap();
    fs::write(
        dir.join("commands/review.md"),
        "---\ndescription: Review code\n---\nReview $ARGUMENTS carefully.\n",
    )
    .unwrap();
    fs::write(dir.join("commands/git/commit.md"), "# Commit the work\n").unwrap();
    fs::write(
        dir.join(".mcp.json"),
        r#"{"mcpServers": {"tool": {"command": "${CLAUDE_PLUGIN_ROOT}/bin/tool"}}}"#,
    )
    .unwrap();
    if native {
        fs::create_dir_all(dir.join("hooks")).unwrap();
        fs::write(dir.join("hooks/hooks.json"), "{}").unwrap();
        fs::create_dir_all(dir.join("agents")).unwrap();
        fs::write(dir.join("agents/a.md"), "x").unwrap();
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_plugins_parts() {
        let root = tempfile::tempdir().unwrap();
        let dir = write_plugin(root.path(), "p@market", false);
        fs::create_dir_all(root.path().join("not-an-id")).unwrap();
        let plugins = scan_plugins(root.path());
        assert_eq!(plugins.len(), 1);
        let plugin = &plugins[0];
        assert_eq!(
            (plugin.name.as_str(), plugin.marketplace.as_str()),
            ("p", "market")
        );
        assert_eq!(plugin.version, "1.2.0");
        assert_eq!(plugin.parts.skills[0].name, "helper");
        let commands: Vec<_> = plugin.parts.commands.iter().map(|c| &c.name).collect();
        assert_eq!(commands, ["git:commit", "review"]);
        assert_eq!(plugin.parts.commands[0].description, "Commit the work");
        assert_eq!(
            plugin.parts.commands[1].render("main.rs"),
            "Review main.rs carefully.\n"
        );
        let tool = &plugin.parts.servers["tool"];
        assert_eq!(
            tool.command.as_deref(),
            Some(dir.join("bin/tool").to_str().unwrap())
        );
        assert!(!plugin.needs_native());
    }

    #[test]
    fn hooks_and_subagents_need_a_native_install() {
        let root = tempfile::tempdir().unwrap();
        write_plugin(root.path(), "p@m", true);
        let plugin = &scan_plugins(root.path())[0];
        assert!(plugin.needs_native());
        assert_eq!(plugin.parts.native, ["subagents", "hooks"]);
    }
}
