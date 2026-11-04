//! Plugin marketplaces: GitHub repositories with a `.claude-plugin/marketplace.json`, which
//! xolkit browses and installs plugins from. And xolkit's own local marketplace in
//! `~/.xolkit/marketplace`, which the agents install native plugins from.

use std::collections::BTreeSet;
use std::fs;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::discover::{
    MAX_PLUGIN, call, check_source, download_folder, github, head_commit, read_file, tree_hash,
    unique, valid_name, valid_plugin_id,
};
use crate::lock::{Lock, Pin, PluginPin};
use crate::ops::Gone;
use crate::{Action, Agent, Env, Kind, Plugin, Profile, Provider, Settings};

/// The name of xolkit's local marketplace in the agents.
pub const MARKETPLACE: &str = "xolkit";

/// A plugin a marketplace offers.
#[derive(Clone, Debug, PartialEq)]
pub struct Offer {
    /// `name@marketplace`, the marketplace named as its `marketplace.json` names it.
    pub id: String,
    pub name: String,
    pub marketplace: String,
    /// The marketplace's repository, `owner/repo`.
    pub marketplace_repo: String,
    pub description: String,
    pub version: String,
    pub source: PluginSource,
}

/// Where a plugin's files are.
#[derive(Clone, Debug, PartialEq)]
pub enum PluginSource {
    /// A folder of a GitHub repository, at `rev` (a branch, tag or commit) or its default branch.
    GitHub {
        repo: String,
        folder: String,
        rev: Option<String>,
    },
    /// Somewhere xolkit can't download from yet, described.
    Unsupported(String),
}

/// Whether `repo` is a GitHub `owner/repo`.
pub fn valid_marketplace(repo: &str) -> bool {
    check_source(repo).is_ok()
}

/// The plugins every marketplace offers. A marketplace that fails is reported, with its error,
/// and skipped.
pub fn fetch_offers(repos: &[String], token: Option<&str>) -> (Vec<Offer>, Vec<String>) {
    let mut offers = Vec::new();
    let mut errors = Vec::new();
    for repo in repos {
        let listed = check_source(repo).and_then(|()| {
            let text = read_file(repo, "HEAD", ".claude-plugin/marketplace.json", token)
                .context("no .claude-plugin/marketplace.json there")?;
            parse_marketplace(repo, &text)
        });
        match listed {
            Ok(listed) => offers.extend(listed),
            Err(e) => errors.push(format!("{repo}: {e:#}")),
        }
    }
    offers.sort_by(|a, b| a.id.cmp(&b.id));
    offers.dedup_by(|a, b| a.id == b.id);
    (offers, errors)
}

fn parse_marketplace(repo: &str, text: &str) -> Result<Vec<Offer>> {
    let json: Value = serde_json::from_str(text).context("marketplace.json is not valid JSON")?;
    let marketplace = json["name"]
        .as_str()
        .filter(|n| valid_name(n))
        .context("marketplace.json has no usable name")?;
    let root = json["metadata"]["pluginRoot"].as_str().unwrap_or_default();
    let plugins = json["plugins"]
        .as_array()
        .context("marketplace.json lists no plugins")?;
    Ok(plugins
        .iter()
        .filter_map(|entry| {
            let name = entry["name"].as_str().filter(|n| valid_name(n))?;
            Some(Offer {
                id: format!("{name}@{marketplace}"),
                name: name.to_owned(),
                marketplace: marketplace.to_owned(),
                marketplace_repo: repo.to_owned(),
                description: entry["description"].as_str().unwrap_or_default().to_owned(),
                version: entry["version"].as_str().unwrap_or_default().to_owned(),
                source: plugin_source(repo, root, &entry["source"]),
            })
        })
        .collect())
}

fn plugin_source(repo: &str, root: &str, source: &Value) -> PluginSource {
    let github_repo = |url: &str| {
        let rest = url
            .strip_prefix("https://github.com/")?
            .trim_end_matches('/')
            .trim_end_matches(".git");
        check_source(rest).is_ok().then(|| rest.to_owned())
    };
    let rev = |key: &str| source[key].as_str().map(str::to_owned);
    match source {
        Value::String(path) => PluginSource::GitHub {
            repo: repo.to_owned(),
            folder: join_folder(root, path),
            rev: None,
        },
        _ => match source["source"].as_str() {
            Some("github")
                if source["repo"]
                    .as_str()
                    .is_some_and(|r| check_source(r).is_ok()) =>
            {
                PluginSource::GitHub {
                    repo: source["repo"].as_str().unwrap_or_default().to_owned(),
                    folder: join_folder("", source["path"].as_str().unwrap_or_default()),
                    rev: rev("sha").or_else(|| rev("ref")),
                }
            }
            Some("url" | "git-subdir") => match source["url"].as_str().and_then(github_repo) {
                Some(repo) => PluginSource::GitHub {
                    repo,
                    folder: join_folder("", source["path"].as_str().unwrap_or_default()),
                    rev: rev("sha").or_else(|| rev("ref")),
                },
                None => PluginSource::Unsupported(format!(
                    "a Git repository outside GitHub ({})",
                    source["url"].as_str().unwrap_or("no URL")
                )),
            },
            Some(other) => PluginSource::Unsupported(format!("a {other} source")),
            None => PluginSource::Unsupported("an unknown source".to_owned()),
        },
    }
}

/// `path` under `root`, both relative to a repository, as `a/b` without `.` parts.
fn join_folder(root: &str, path: &str) -> String {
    root.split('/')
        .chain(path.split('/'))
        .filter(|p| !p.is_empty() && *p != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// A plugin downloaded into a staging directory, ready to install.
#[derive(Clone, Debug)]
pub struct FetchedPlugin {
    pub plugin: Plugin,
    pub pin: PluginPin,
}

impl FetchedPlugin {
    /// Removes the staging directory.
    pub fn discard(&self) {
        if let Some(parent) = self.plugin.dir.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }
}

/// Downloads an offered plugin into a staging directory.
pub fn fetch_plugin(env: &Env, offer: &Offer, token: Option<&str>) -> Result<FetchedPlugin> {
    fetch_plugin_at(env, &offer.id, offer_pin(offer, token)?, token)
}

fn offer_pin(offer: &Offer, token: Option<&str>) -> Result<PluginPin> {
    let PluginSource::GitHub { repo, folder, rev } = &offer.source else {
        let PluginSource::Unsupported(what) = &offer.source else {
            unreachable!()
        };
        bail!("xolkit can't install plugins from {what}");
    };
    ensure!(
        folder.split('/').all(|p| p != ".."),
        "unsafe plugin folder: {folder}"
    );
    let commit = match rev {
        Some(rev) if rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit()) => rev.clone(),
        Some(rev) => commit_of(repo, rev, token)?,
        None => head_commit(repo, token)?,
    };
    let tree = tree_hash(repo, &commit, folder, token)?;
    Ok(PluginPin {
        pin: Pin {
            source: repo.clone(),
            folder: folder.clone(),
            commit,
            tree,
            ..Pin::default()
        },
        marketplace: offer.marketplace_repo.clone(),
        version: offer.version.clone(),
        ..PluginPin::default()
    })
}

/// The current marketplace source of a newer installed plugin.
#[derive(Clone, Debug)]
pub struct PluginUpdate {
    pub id: String,
    pub pin: PluginPin,
}

/// Checks the marketplace again, honoring its current branch, tag or pinned commit.
pub fn check_plugin_update(
    env: &Env,
    id: &str,
    token: Option<&str>,
) -> Result<Option<PluginUpdate>> {
    ensure!(valid_plugin_id(id), "unsafe plugin id: {id}");
    let lock = Lock::read(&env.lock_file())?;
    let Some(installed) = lock.plugins.get(id).filter(|p| !p.marketplace.is_empty()) else {
        return Ok(None);
    };
    check_source(&installed.marketplace)?;
    let text = read_file(
        &installed.marketplace,
        "HEAD",
        ".claude-plugin/marketplace.json",
        token,
    )?;
    let offer = parse_marketplace(&installed.marketplace, &text)?
        .into_iter()
        .find(|offer| offer.id == id)
        .context("the plugin is no longer listed in its marketplace")?;
    let pin = offer_pin(&offer, token)?;
    Ok((pin.pin.tree != installed.pin.tree).then(|| PluginUpdate {
        id: id.to_owned(),
        pin,
    }))
}

/// Downloads exactly the plugin revision whose contents were checked.
pub fn fetch_plugin_update(
    env: &Env,
    update: &PluginUpdate,
    token: Option<&str>,
) -> Result<FetchedPlugin> {
    fetch_plugin_at(env, &update.id, update.pin.clone(), token)
}

/// Replaces a managed plugin and refreshes its native installs, preserving profile switches.
pub fn update_plugin(
    env: &Env,
    settings: &Settings,
    lock: &Lock,
    fetched: &FetchedPlugin,
) -> Result<Vec<Action>> {
    let plugin = &fetched.plugin;
    ensure!(
        valid_plugin_id(&plugin.id),
        "unsafe plugin id: {}",
        plugin.id
    );
    let installed = lock
        .plugins
        .get(&plugin.id)
        .context("the plugin is not managed by xolkit")?;
    let target = env.plugins_dir().join(&plugin.id);
    ensure!(
        fs::symlink_metadata(&target).is_ok_and(|m| m.is_dir()),
        "{} is not a folder xolkit can replace",
        env.tilde(&target)
    );
    let previous = Plugin::read(&target).context("could not read the installed plugin")?;
    let old = env.staging_dir().join(format!("{}-old", unique()));
    let mut actions = Vec::new();
    let mut after = lock.clone();
    for provider in &installed.native {
        actions.extend(native_uninstall(env, &after, &previous, *provider));
        after
            .plugins
            .get_mut(&plugin.id)
            .unwrap()
            .native
            .remove(provider);
    }
    let mut pin = fetched.pin.clone();
    pin.pin.installed_at = installed.pin.installed_at.clone();
    pin.pin.updated_at = humantime::format_rfc3339_millis(std::time::SystemTime::now()).to_string();
    pin.native.clear();
    if pin.version.is_empty() {
        pin.version = plugin.version.clone();
    }
    after.plugins.insert(plugin.id.clone(), pin);
    if plugin.needs_native() {
        let updated = Plugin {
            dir: target.clone(),
            ..plugin.clone()
        };
        let mut providers = installed.native.clone();
        for provider in Provider::ALL {
            if settings
                .active()
                .is_on(Kind::Plugin, &plugin.id, Agent::from(provider))
            {
                providers.insert(provider);
            }
        }
        for provider in providers {
            // Record intent before invoking a new provider: a CLI can install successfully
            // and still report failure. Retry and uninstall must own that possible install.
            if !installed.native.contains(&provider) {
                actions.push(Action::SetNative {
                    id: plugin.id.clone(),
                    provider,
                    installed: true,
                });
            }
            actions.extend(native_install(env, &after, &updated, provider)?);
            after
                .plugins
                .get_mut(&plugin.id)
                .unwrap()
                .native
                .insert(provider);
            actions.push(native_toggle(
                &updated,
                provider,
                settings
                    .active()
                    .is_on(Kind::Plugin, &plugin.id, Agent::from(provider)),
            ));
        }
    }
    // Keep the old content pin and all attempted native providers until completion. A failed
    // refresh still offers the update and retains ownership for retry or uninstall cleanup.
    actions.retain(|action| {
        !matches!(
            action,
            Action::SetNative {
                installed: false,
                ..
            }
        )
    });
    actions.push(Action::PinPlugin {
        id: plugin.id.clone(),
        pin: after.plugins.get(&plugin.id).cloned(),
    });
    Ok(vec![
        Action::ReplaceDir {
            from: plugin.dir.clone(),
            to: target,
            backup: old.clone(),
            steps: actions,
        },
        Action::Remove { path: old },
    ])
}

/// Downloads plugin `id` as `pin` has it, as a shared profile pins it. `token` is needed for a
/// private repository.
pub fn fetch_plugin_at(
    env: &Env,
    id: &str,
    pin: PluginPin,
    token: Option<&str>,
) -> Result<FetchedPlugin> {
    ensure!(valid_plugin_id(id), "unsafe plugin id: {id}");
    let (staging, _) = download_folder(
        env,
        &pin.pin.source,
        &pin.pin.commit,
        &pin.pin.folder,
        MAX_PLUGIN,
        token,
    )?;
    // Unpack under a folder named by the id, so it reads like an installed plugin.
    let holder = staging.with_extension("plugin");
    let dir = holder.join(id);
    let moved = fs::create_dir_all(&holder).and_then(|()| fs::rename(&staging, &dir));
    let plugin = moved.ok().and_then(|()| Plugin::read_as(&dir, id));
    let Some(plugin) = plugin else {
        let _ = fs::remove_dir_all(&staging);
        let _ = fs::remove_dir_all(&holder);
        bail!("{id} is not a plugin xolkit can read");
    };
    Ok(FetchedPlugin { plugin, pin })
}

fn commit_of(repo: &str, rev: &str, token: Option<&str>) -> Result<String> {
    ensure!(
        rev.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
            && rev
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != ".."),
        "unsafe Git ref: {rev}"
    );
    let request = github(
        token,
        &format!("https://api.github.com/repos/{repo}/commits/{rev}"),
    )
    .header("Accept", "application/vnd.github.sha");
    let commit = call(request, token)
        .with_context(|| format!("could not find {rev} in github.com/{repo}"))?
        .body_mut()
        .read_to_string()?;
    let commit = commit.trim().to_owned();
    ensure!(
        commit.len() == 40 && commit.chars().all(|c| c.is_ascii_hexdigit()),
        "unexpected commit id from GitHub"
    );
    Ok(commit)
}

/// Moves a fetched plugin into the library, records it, turns it on for `agents` in the active
/// profile, and installs it natively for those of them it needs that for.
pub fn install_plugin(
    env: &Env,
    settings: &Settings,
    lock: &Lock,
    fetched: &FetchedPlugin,
    agents: &[Agent],
) -> Result<Vec<Action>> {
    let plugin = &fetched.plugin;
    ensure!(
        valid_plugin_id(&plugin.id),
        "unsafe plugin id: {}",
        plugin.id
    );
    let target = env.plugins_dir().join(&plugin.id);
    ensure!(
        fs::symlink_metadata(&target).is_err(),
        "{} is installed already",
        plugin.id
    );
    let now = humantime::format_rfc3339_millis(std::time::SystemTime::now()).to_string();
    let mut pin = fetched.pin.clone();
    pin.pin.installed_at = now.clone();
    pin.pin.updated_at = now;
    if pin.version.is_empty() {
        pin.version = plugin.version.clone();
    }
    let mut settings = settings.clone();
    for &agent in agents {
        settings
            .active_mut()
            .set(Kind::Plugin, &plugin.id, agent, true);
    }
    let mut actions = vec![
        Action::Rename {
            from: plugin.dir.clone(),
            to: target.clone(),
        },
        Action::PinPlugin {
            id: plugin.id.clone(),
            pin: Some(pin),
        },
        Action::SaveSettings {
            settings: Box::new(settings),
        },
    ];
    if plugin.needs_native() {
        let installed = Plugin {
            dir: target,
            ..plugin.clone()
        };
        let mut lock = lock.clone();
        lock.plugins.entry(plugin.id.clone()).or_default();
        for provider in agents.iter().filter_map(|a| a.provider()) {
            actions.extend(native_install(env, &lock, &installed, provider)?);
            lock.plugins
                .entry(plugin.id.clone())
                .or_default()
                .native
                .insert(provider);
        }
    }
    Ok(actions)
}

/// The id an agent knows a native plugin by.
fn native_id(plugin: &Plugin) -> String {
    format!("{}@{MARKETPLACE}", plugin.name)
}

/// `marketplace.json` listing `names`, for the agents to install from.
fn marketplace_json(names: &BTreeSet<String>) -> String {
    let plugins: Vec<Value> = names
        .iter()
        .map(|name| json!({"name": name, "source": format!("./plugins/{name}")}))
        .collect();
    let json = json!({
        "name": MARKETPLACE,
        "owner": {"name": "xolkit"},
        "metadata": {"description": "Plugins xolkit installs natively. Managed by xolkit."},
        "plugins": plugins,
    });
    serde_json::to_string_pretty(&json).unwrap_or_default() + "\n"
}

/// Names of the plugins in xolkit's marketplace: those some agent has natively.
fn native_names(lock: &Lock) -> BTreeSet<String> {
    lock.plugins
        .iter()
        .filter(|(_, pin)| !pin.native.is_empty())
        .filter_map(|(id, _)| Some(id.split_once('@')?.0.to_owned()))
        .collect()
}

/// Steps that install an installed plugin natively in `provider`: through xolkit's marketplace,
/// which is added to the agent the first time.
pub fn native_install(
    env: &Env,
    lock: &Lock,
    plugin: &Plugin,
    provider: Provider,
) -> Result<Vec<Action>> {
    let clash = lock.plugins.iter().any(|(id, pin)| {
        id != &plugin.id
            && !pin.native.is_empty()
            && id.split_once('@').map(|s| s.0) == Some(&plugin.name)
    });
    ensure!(
        !clash,
        "another plugin named {} is installed natively already",
        plugin.name
    );
    let marketplace = env.marketplace_dir();
    let copy = marketplace.join("plugins").join(&plugin.name);
    let mut names = native_names(lock);
    names.insert(plugin.name.clone());
    let added = lock
        .plugins
        .values()
        .any(|pin| pin.native.contains(&provider));
    let mut actions = Vec::new();
    if lock.native(&plugin.id).is_empty() {
        actions.push(Action::Remove { path: copy.clone() });
        actions.push(Action::Copy {
            from: plugin.dir.clone(),
            to: copy,
        });
    }
    actions.push(Action::WriteFile {
        path: marketplace.join(".claude-plugin").join("marketplace.json"),
        contents: marketplace_json(&names),
    });
    let dir = marketplace.to_string_lossy().into_owned();
    let run = |args: &[&str]| Action::Run {
        provider,
        args: args.iter().map(|a| (*a).to_owned()).collect(),
    };
    let id = native_id(plugin);
    match provider {
        Provider::Claude => {
            actions.push(if added {
                run(&["plugin", "marketplace", "update", MARKETPLACE])
            } else {
                run(&["plugin", "marketplace", "add", &dir])
            });
            actions.push(run(&["plugin", "install", &id, "--scope", "user"]));
        }
        Provider::Codex => {
            if !added {
                actions.push(run(&["plugin", "marketplace", "add", &dir]));
            }
            actions.push(run(&["plugin", "add", &id]));
        }
    }
    actions.push(Action::SetNative {
        id: plugin.id.clone(),
        provider,
        installed: true,
    });
    Ok(actions)
}

/// Turns a native plugin on or off in `provider`.
pub fn native_toggle(plugin: &Plugin, provider: Provider, enabled: bool) -> Action {
    let id = native_id(plugin);
    match provider {
        Provider::Claude => Action::SetClaudePlugin { id, enabled },
        Provider::Codex => Action::SetCodexPlugin { id, enabled },
    }
}

/// Whether `provider` has plugin `id` installed (at user scope, for Claude Code), and if so,
/// whether it is on: `None` when it isn't installed. Fails when the agent's list can't be read
/// in full, so a partial answer never passes for "not installed".
pub(crate) fn native_state(env: &Env, provider: Provider, id: &str) -> Result<Option<bool>> {
    let args = ["plugin", "list", "--json"].map(String::from);
    let list: Value = serde_json::from_str(&env.run(provider.cli(), &args)?)
        .context("the agent's plugin list isn't valid JSON")?;
    let (entries, key) = match provider {
        Provider::Claude => (list.as_array(), "id"),
        // Codex lists what it offers under `available`; only `installed` counts.
        Provider::Codex => (list["installed"].as_array(), "pluginId"),
    };
    let entries = entries.context("unexpected plugin list from the agent")?;
    // Every entry must say what it is, so a malformed one can't hide the plugin.
    let mut found = None;
    for entry in entries {
        let listed = entry[key]
            .as_str()
            .context("the agent's plugin list has an entry without an id")?;
        let user = match provider {
            Provider::Claude => {
                entry["scope"]
                    .as_str()
                    .context("the agent's plugin list has an entry without a scope")?
                    == "user"
            }
            Provider::Codex => true,
        };
        if listed == id && user {
            let on = entry["enabled"]
                .as_bool()
                .context("the agent's plugin list doesn't say whether the plugin is on")?;
            found = Some(on);
        }
    }
    Ok(found)
}

/// Steps that uninstall a native plugin from `provider`, and take xolkit's marketplace out of
/// the agent when it was the last.
fn native_uninstall(env: &Env, lock: &Lock, plugin: &Plugin, provider: Provider) -> Vec<Action> {
    let id = native_id(plugin);
    let run = |args: &[&str], gone: Gone| Action::Undo {
        provider,
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        gone,
    };
    let gone = || Gone::Plugin { id: id.clone() };
    let mut actions = match provider {
        Provider::Claude => vec![run(
            &["plugin", "uninstall", &id, "--scope", "user"],
            gone(),
        )],
        Provider::Codex => vec![
            run(&["plugin", "remove", &id], gone()),
            Action::ForgetCodexHooks { id: id.clone() },
        ],
    };
    actions.push(Action::SetNative {
        id: plugin.id.clone(),
        provider,
        installed: false,
    });
    let mut after = lock.clone();
    if let Some(pin) = after.plugins.get_mut(&plugin.id) {
        pin.native.remove(&provider);
    }
    if !after
        .plugins
        .values()
        .any(|pin| pin.native.contains(&provider))
    {
        actions.push(run(
            &["plugin", "marketplace", "remove", MARKETPLACE],
            Gone::Marketplace,
        ));
    }
    if after.native(&plugin.id).is_empty() {
        let marketplace = env.marketplace_dir();
        actions.push(Action::Remove {
            path: marketplace.join("plugins").join(&plugin.name),
        });
        actions.push(Action::WriteFile {
            path: marketplace.join(".claude-plugin").join("marketplace.json"),
            contents: marketplace_json(&native_names(&after)),
        });
    }
    actions
}

/// Deletes a plugin: from every agent that has it natively, the library and every profile.
pub fn uninstall_plugin(
    env: &Env,
    settings: &Settings,
    lock: &Lock,
    plugin: &Plugin,
) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut lock = lock.clone();
    for provider in lock.native(&plugin.id) {
        actions.extend(native_uninstall(env, &lock, plugin, provider));
        if let Some(pin) = lock.plugins.get_mut(&plugin.id) {
            pin.native.remove(&provider);
        }
    }
    let mut settings = settings.clone();
    settings.forget(Kind::Plugin, &plugin.id);
    actions.extend([
        Action::Remove {
            path: plugin.dir.clone(),
        },
        Action::PinPlugin {
            id: plugin.id.clone(),
            pin: None,
        },
        Action::SaveSettings {
            settings: Box::new(settings),
        },
    ]);
    actions
}

/// The native plugin toggles a switch from `from` to `to` changes, to show before it runs.
/// Served plugins need none: the daemon serves the new profile at once.
pub fn native_changes(
    lock: &Lock,
    plugins: &[Plugin],
    from: &Profile,
    to: &Profile,
) -> Vec<Action> {
    let mut steps = Vec::new();
    for plugin in plugins {
        for provider in lock.native(&plugin.id) {
            let agent = Agent::from(provider);
            let want = to.is_on(Kind::Plugin, &plugin.id, agent);
            if want != from.is_on(Kind::Plugin, &plugin.id, agent) {
                steps.push(native_toggle(plugin, provider, want));
            }
        }
    }
    steps
}

/// Steps that set every native plugin as `to` has it, changed or not: the toggles are
/// idempotent, so a switch that failed halfway is set right by the next.
pub fn native_steps(lock: &Lock, plugins: &[Plugin], to: &Profile) -> Vec<Action> {
    let mut steps = Vec::new();
    for plugin in plugins {
        for provider in lock.native(&plugin.id) {
            let want = to.is_on(Kind::Plugin, &plugin.id, Agent::from(provider));
            steps.push(native_toggle(plugin, provider, want));
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::write_plugin;

    #[test]
    fn reads_marketplace_sources() {
        let text = r#"{
            "name": "tools",
            "metadata": {"pluginRoot": "./plugins"},
            "plugins": [
                {"name": "local", "source": "./local", "description": "Here", "version": "1.0"},
                {"name": "gh", "source": {"source": "github", "repo": "o/other", "ref": "v2"}},
                {"name": "url", "source": {"source": "url", "url": "https://github.com/o/u.git"}},
                {"name": "lab", "source": {"source": "url", "url": "https://gitlab.com/o/l.git"}},
                {"name": "bad name!", "source": "./x"}
            ]
        }"#;
        let offers = parse_marketplace("o/market", text).unwrap();
        let ids: Vec<_> = offers.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["local@tools", "gh@tools", "url@tools", "lab@tools"]);
        assert_eq!(
            offers[0].source,
            PluginSource::GitHub {
                repo: "o/market".into(),
                folder: "plugins/local".into(),
                rev: None
            }
        );
        assert_eq!(
            offers[1].source,
            PluginSource::GitHub {
                repo: "o/other".into(),
                folder: String::new(),
                rev: Some("v2".into())
            }
        );
        assert!(matches!(offers[2].source, PluginSource::GitHub { ref repo, .. } if repo == "o/u"));
        assert!(matches!(offers[3].source, PluginSource::Unsupported(_)));
        assert_eq!(offers[0].marketplace_repo, "o/market");
        assert!(parse_marketplace("o/m", "{}").is_err());
    }

    fn installed(env: &Env, native: bool) -> Plugin {
        let dir = write_plugin(&env.plugins_dir(), "p@m", native);
        Plugin::read(&dir).unwrap()
    }

    fn flattened(actions: &[Action]) -> Vec<Action> {
        actions
            .iter()
            .flat_map(|action| match action {
                Action::ReplaceDir { steps, .. } => flattened(steps),
                other => vec![other.clone()],
            })
            .collect()
    }

    fn runs(actions: &[Action]) -> Vec<String> {
        flattened(actions)
            .iter()
            .filter_map(|a| match a {
                Action::Run { provider, args } | Action::Undo { provider, args, .. } => {
                    Some(format!("{} {}", provider.cli(), args.join(" ")))
                }
                _ => None,
            })
            .collect()
    }

    fn fetched_update(env: &Env, native: bool) -> FetchedPlugin {
        let dir = write_plugin(&env.staging_dir(), "p@m", native);
        fs::write(dir.join("commands/new.md"), "New command").unwrap();
        FetchedPlugin {
            plugin: Plugin::read(&dir).unwrap(),
            pin: PluginPin {
                pin: Pin {
                    source: "o/r".into(),
                    commit: "new".into(),
                    tree: "new-tree".into(),
                    ..Pin::default()
                },
                marketplace: "o/m".into(),
                ..PluginPin::default()
            },
        }
    }

    #[test]
    fn update_replaces_files_and_pin_without_changing_profiles() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        installed(&env, false);
        let mut lock = Lock::default();
        lock.plugins.insert(
            "p@m".into(),
            PluginPin {
                pin: Pin {
                    installed_at: "original".into(),
                    ..Pin::default()
                },
                ..PluginPin::default()
            },
        );
        lock.write(&env.lock_file()).unwrap();
        let settings = Settings::default();
        let fetched = fetched_update(&env, false);
        let actions = update_plugin(&env, &settings, &lock, &fetched).unwrap();
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::SaveSettings { .. }))
        );
        crate::execute(&env, &actions).unwrap();
        assert!(env.plugins_dir().join("p@m/commands/new.md").is_file());
        assert!(!fetched.plugin.dir.exists());
        let pin = Lock::read(&env.lock_file()).unwrap().plugins["p@m"].clone();
        assert_eq!(pin.pin.commit, "new");
        assert_eq!(pin.pin.tree, "new-tree");
        assert_eq!(pin.pin.installed_at, "original");
        assert!(!pin.pin.updated_at.is_empty());
        assert_eq!(pin.version, "1.2.0");
    }

    #[test]
    fn update_refreshes_both_native_copies_and_restores_switches() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        installed(&env, true);
        let mut lock = Lock::default();
        lock.plugins.insert(
            "p@m".into(),
            PluginPin {
                native: BTreeSet::from(Provider::ALL),
                ..PluginPin::default()
            },
        );
        let mut settings = Settings::default();
        settings.active_mut().set_agent(Agent::Claude, true);
        settings
            .active_mut()
            .set(Kind::Plugin, "p@m", Agent::Claude, true);
        let actions = update_plugin(&env, &settings, &lock, &fetched_update(&env, true)).unwrap();
        let commands = runs(&actions);
        for command in [
            "claude plugin uninstall p@xolkit --scope user",
            "codex plugin remove p@xolkit",
            "claude plugin install p@xolkit --scope user",
            "codex plugin add p@xolkit",
        ] {
            assert!(commands.iter().any(|c| c == command), "{commands:?}");
        }
        assert!(flattened(&actions).contains(&Action::SetClaudePlugin {
            id: "p@xolkit".into(),
            enabled: true
        }));
        assert!(flattened(&actions).contains(&Action::SetCodexPlugin {
            id: "p@xolkit".into(),
            enabled: false
        }));
        assert_eq!(
            flattened(&actions)
                .iter()
                .filter(|a| matches!(a, Action::Copy { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn update_removes_native_installs_when_no_longer_needed() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        installed(&env, true);
        let mut lock = Lock::default();
        lock.plugins.insert(
            "p@m".into(),
            PluginPin {
                native: BTreeSet::from([Provider::Claude]),
                ..PluginPin::default()
            },
        );
        let actions = update_plugin(
            &env,
            &Settings::default(),
            &lock,
            &fetched_update(&env, false),
        )
        .unwrap();
        assert!(
            runs(&actions)
                .iter()
                .any(|c| c == "claude plugin uninstall p@xolkit --scope user")
        );
        assert!(!actions.iter().any(|a| matches!(a, Action::Run { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn failed_native_refresh_keeps_old_pin_and_all_providers_for_retry() {
        use std::os::unix::fs::PermissionsExt;
        for (original_native, failing_provider, failing_verb) in [
            (BTreeSet::from(Provider::ALL), "codex", "remove"),
            (BTreeSet::from(Provider::ALL), "claude", "install"),
            (BTreeSet::new(), "codex", "add"),
            (BTreeSet::from([Provider::Claude]), "codex", "add"),
        ] {
            let home = tempfile::tempdir().unwrap();
            let mut env = Env::at(home.path());
            installed(&env, true);
            let bin = home.path().join("bin");
            fs::create_dir(&bin).unwrap();
            for provider in ["claude", "codex"] {
                let cli = bin.join(provider);
                let script = if provider == failing_provider {
                    format!("#!/bin/sh\nif [ \"$2\" = {failing_verb} ]; then exit 1; fi\nexit 0\n")
                } else {
                    "#!/bin/sh\nexit 0\n".to_owned()
                };
                fs::write(&cli, script).unwrap();
                fs::set_permissions(&cli, fs::Permissions::from_mode(0o755)).unwrap();
            }
            env.path = Some(bin.clone().into_os_string());
            fs::create_dir_all(&env.codex_dir).unwrap();
            fs::write(
                env.codex_config(),
                "[plugins.\"p@xolkit\"]\nenabled = true\n",
            )
            .unwrap();
            let mut settings = Settings::default();
            settings.active_mut().set_agent(Agent::Claude, true);
            settings
                .active_mut()
                .set(Kind::Plugin, "p@m", Agent::Claude, true);
            let codex_on = !original_native.contains(&Provider::Codex);
            settings.active_mut().set_agent(Agent::Codex, true);
            settings
                .active_mut()
                .set(Kind::Plugin, "p@m", Agent::Codex, codex_on);
            let mut lock = Lock::default();
            lock.plugins.insert(
                "p@m".into(),
                PluginPin {
                    pin: Pin {
                        tree: "old-tree".into(),
                        ..Pin::default()
                    },
                    native: original_native,
                    ..PluginPin::default()
                },
            );
            lock.write(&env.lock_file()).unwrap();
            let actions =
                update_plugin(&env, &settings, &lock, &fetched_update(&env, true)).unwrap();
            assert!(crate::execute(&env, &actions).is_err());
            let interrupted = Lock::read(&env.lock_file()).unwrap();
            assert_eq!(interrupted.plugins["p@m"].pin.tree, "old-tree");
            assert_eq!(
                interrupted.plugins["p@m"].native,
                BTreeSet::from(Provider::ALL)
            );
            let current = Plugin::read(&env.plugins_dir().join("p@m")).unwrap();
            let cleanup = uninstall_plugin(&env, &settings, &interrupted, &current);
            assert!(
                runs(&cleanup)
                    .iter()
                    .any(|run| run == "claude plugin uninstall p@xolkit --scope user")
            );
            assert!(
                runs(&cleanup)
                    .iter()
                    .any(|run| run == "codex plugin remove p@xolkit")
            );
            assert!(!env.plugins_dir().join("p@m/commands/new.md").exists());
            fs::write(bin.join(failing_provider), "#!/bin/sh\nexit 0\n").unwrap();
            fs::write(
                bin.join("claude"),
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> claude.log\nexit 0\n",
            )
            .unwrap();
            let retry = update_plugin(&env, &settings, &lock, &fetched_update(&env, true)).unwrap();
            crate::execute(&env, &retry).unwrap();
            let updated = Lock::read(&env.lock_file()).unwrap();
            assert_eq!(updated.plugins["p@m"].native, BTreeSet::from(Provider::ALL));
            assert_eq!(updated.plugins["p@m"].pin.tree, "new-tree");
            let codex = crate::config::read_toml(&env.codex_config()).unwrap();
            assert_eq!(
                codex["plugins"]["p@xolkit"]["enabled"].as_bool(),
                Some(codex_on)
            );
            let calls = fs::read_to_string(home.path().join("claude.log")).unwrap();
            assert!(
                calls
                    .lines()
                    .any(|line| line == "plugin enable p@xolkit --scope user")
            );
        }
    }

    #[test]
    fn failed_pin_write_keeps_update_available() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        installed(&env, false);
        let mut lock = Lock::default();
        lock.plugins.insert(
            "p@m".into(),
            PluginPin {
                pin: Pin {
                    tree: "old-tree".into(),
                    ..Pin::default()
                },
                ..PluginPin::default()
            },
        );
        lock.write(&env.lock_file()).unwrap();
        // Block the atomic lock write while leaving its existing contents readable.
        fs::create_dir(env.xolkit_dir.join(".lock.json.xolkit-tmp")).unwrap();
        let actions = update_plugin(
            &env,
            &Settings::default(),
            &lock,
            &fetched_update(&env, false),
        )
        .unwrap();
        assert!(crate::execute(&env, &actions).is_err());
        assert_eq!(Lock::read(&env.lock_file()).unwrap(), lock);
        assert!(!env.plugins_dir().join("p@m/commands/new.md").exists());
        fs::remove_dir(env.xolkit_dir.join(".lock.json.xolkit-tmp")).unwrap();
        let retry = update_plugin(
            &env,
            &Settings::default(),
            &lock,
            &fetched_update(&env, false),
        )
        .unwrap();
        crate::execute(&env, &retry).unwrap();
        assert_eq!(
            Lock::read(&env.lock_file()).unwrap().plugins["p@m"]
                .pin
                .tree,
            "new-tree"
        );
    }

    #[test]
    fn update_refuses_to_replace_a_link() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let fetched = fetched_update(&env, false);
        fs::create_dir_all(env.plugins_dir()).unwrap();
        crate::testing::link_dir(&fetched.plugin.dir, env.plugins_dir().join("p@m"));
        let mut lock = Lock::default();
        lock.plugins.insert("p@m".into(), PluginPin::default());
        assert!(update_plugin(&env, &Settings::default(), &lock, &fetched).is_err());
    }

    #[test]
    fn installs_natively_through_xolkits_marketplace() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let plugin = installed(&env, true);
        let lock = Lock::default();
        let claude = native_install(&env, &lock, &plugin, Provider::Claude).unwrap();
        let dir = env.marketplace_dir().to_string_lossy().into_owned();
        assert_eq!(
            runs(&claude),
            [
                format!("claude plugin marketplace add {dir}"),
                "claude plugin install p@xolkit --scope user".to_owned()
            ]
        );
        // Only the plugin's files and the marketplace listing are written; no agent runs here.
        let local: Vec<Action> = claude
            .iter()
            .filter(|a| !matches!(a, Action::Run { .. } | Action::Undo { .. }))
            .cloned()
            .collect();
        crate::execute(&env, &local).unwrap();
        let listing = fs::read_to_string(
            env.marketplace_dir()
                .join(".claude-plugin/marketplace.json"),
        )
        .unwrap();
        assert!(listing.contains("\"./plugins/p\""), "{listing}");
        assert!(
            env.marketplace_dir()
                .join("plugins/p/hooks/hooks.json")
                .is_file()
        );
        let lock = Lock::read(&env.lock_file()).unwrap();
        assert!(lock.native("p@m").contains(&Provider::Claude));

        // A second agent reuses the copy.
        let codex = native_install(&env, &lock, &plugin, Provider::Codex).unwrap();
        assert!(!codex.iter().any(|a| matches!(a, Action::Copy { .. })));
        assert_eq!(
            runs(&codex),
            [
                format!("codex plugin marketplace add {dir}"),
                "codex plugin add p@xolkit".to_owned()
            ]
        );
    }

    #[test]
    fn uninstall_removes_native_installs_and_the_marketplace() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let plugin = installed(&env, true);
        let mut lock = Lock::default();
        lock.plugins.insert(
            "p@m".into(),
            PluginPin {
                native: BTreeSet::from([Provider::Claude]),
                ..PluginPin::default()
            },
        );
        let mut settings = Settings::default();
        settings
            .active_mut()
            .set(Kind::Plugin, "p@m", Agent::Claude, true);
        let actions = uninstall_plugin(&env, &settings, &lock, &plugin);
        assert_eq!(
            runs(&actions),
            [
                "claude plugin uninstall p@xolkit --scope user",
                "claude plugin marketplace remove xolkit"
            ]
        );
        assert!(actions.contains(&Action::Remove {
            path: plugin.dir.clone()
        }));
        let Some(Action::SaveSettings { settings }) = actions.last() else {
            panic!("{actions:?}");
        };
        assert!(settings.active().plugins.is_empty());
    }

    #[test]
    fn profile_switches_toggle_native_plugins_only() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let plugin = installed(&env, true);
        let mut lock = Lock::default();
        lock.plugins.insert(
            "p@m".into(),
            PluginPin {
                native: BTreeSet::from([Provider::Codex]),
                ..PluginPin::default()
            },
        );
        let mut on = Profile::new("on", vec![Agent::Claude, Agent::Codex]);
        on.set(Kind::Plugin, "p@m", Agent::Codex, true);
        on.set(Kind::Plugin, "p@m", Agent::Claude, true);
        let off = Profile::new("off", vec![Agent::Claude, Agent::Codex]);
        assert_eq!(
            native_changes(&lock, std::slice::from_ref(&plugin), &on, &off),
            [Action::SetCodexPlugin {
                id: "p@xolkit".into(),
                enabled: false
            }]
        );
        assert!(native_changes(&lock, std::slice::from_ref(&plugin), &on, &on).is_empty());
        // Setting a profile sets every native plugin, so a half-done switch is set right.
        assert_eq!(native_steps(&lock, &[plugin], &on).len(), 1);
    }

    #[test]
    fn unmanaged_plugins_need_no_update_check() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        assert!(check_plugin_update(&env, "p@m", None).unwrap().is_none());
        let mut lock = Lock::default();
        lock.plugins.insert("p@m".into(), PluginPin::default());
        lock.write(&env.lock_file()).unwrap();
        assert!(check_plugin_update(&env, "p@m", None).unwrap().is_none());
    }

    /// Reads Anthropic's official marketplace and downloads one of its plugins.
    #[test]
    #[ignore = "uses the network"]
    fn reads_a_real_marketplace() {
        let (offers, errors) = fetch_offers(&["anthropics/claude-plugins-official".into()], None);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(offers.len() > 5, "{} offers", offers.len());
        let offer = offers
            .iter()
            .find(|o| matches!(&o.source, PluginSource::GitHub { repo, .. } if repo == "anthropics/claude-plugins-official"))
            .expect("a plugin in the marketplace's own repository");
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let fetched = fetch_plugin(&env, offer, None).unwrap();
        assert_eq!(fetched.plugin.id, offer.id);
        assert_eq!(fetched.pin.pin.commit.len(), 40);
        // Record the current contents, then simulate an older install without a version bump.
        let mut lock = Lock::default();
        lock.plugins.insert(offer.id.clone(), fetched.pin.clone());
        lock.write(&env.lock_file()).unwrap();
        assert!(
            check_plugin_update(&env, &offer.id, None)
                .unwrap()
                .is_none()
        );
        lock.plugins.get_mut(&offer.id).unwrap().pin.tree = "older-tree".into();
        lock.write(&env.lock_file()).unwrap();
        let update = check_plugin_update(&env, &offer.id, None).unwrap().unwrap();
        assert_eq!(update.pin.pin.tree, fetched.pin.pin.tree);
        fetched.discard();
        let download = fetch_plugin_update(&env, &update, None).unwrap();
        assert_eq!(download.pin.pin.commit, update.pin.pin.commit);
        download.discard();
    }

    /// Reads a private marketplace, `XOLKIT_TEST_PRIVATE_MARKETPLACE` (`owner/repo`), with the
    /// GitHub CLI's token, and downloads one of its plugins from its own repository; and says
    /// without a token that it may be private. Skipped when the variable isn't set.
    #[test]
    #[ignore = "uses the network"]
    fn reads_a_private_marketplace() {
        let Ok(repo) = std::env::var("XOLKIT_TEST_PRIVATE_MARKETPLACE") else {
            return;
        };
        let gh = std::process::Command::new("gh")
            .args(["auth", "token"])
            .output()
            .unwrap();
        let token = String::from_utf8(gh.stdout).unwrap().trim().to_owned();
        let (offers, errors) = fetch_offers(std::slice::from_ref(&repo), Some(&token));
        assert!(errors.is_empty(), "{errors:?}");
        let offer = offers
            .iter()
            .find(|o| matches!(&o.source, PluginSource::GitHub { repo: r, .. } if *r == repo))
            .expect("a plugin in the marketplace's own repository");
        let home = tempfile::tempdir().unwrap();
        let fetched = fetch_plugin(&Env::at(home.path()), offer, Some(&token)).unwrap();
        assert_eq!(fetched.plugin.id, offer.id);
        fetched.discard();
        let (_, errors) = fetch_offers(std::slice::from_ref(&repo), None);
        assert!(
            errors[0].contains("private and needs a token"),
            "{errors:?}"
        );
    }
}
