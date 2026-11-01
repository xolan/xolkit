//! Sharing profiles. A code (`xolkit1:` and the profile as deflated, base64url JSON) lists each
//! skill and plugin by its GitHub source and commit, never file contents, so it stays short
//! enough to paste. A `.xolkit` file also carries the skills that exist only on this computer.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use crate::discover::{extract, fetch_skill_at, unique, valid_name};
use crate::lock::{Pin, PluginPin};
use crate::marketplace::{FetchedPlugin, fetch_plugin_at};
use crate::servers::valid_server_name;
use crate::{
    Action, Agent, Env, FetchedSkill, Kind, Library, Profile, ServerConfig, Settings,
};

const PREFIX: &str = "xolkit1:";
const LINK_PREFIX: &str = "xolkit://import/";
/// Largest decoded profile, so a hostile code can't exhaust memory.
const MAX_JSON: u64 = 1024 * 1024;
/// Largest `.xolkit` file.
const MAX_FILE: u64 = 50 * 1024 * 1024;
/// Commits are shared by their first 12 characters, resolved again on import.
const SHORT_COMMIT: usize = 12;

/// A shared profile.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Shared {
    #[serde(rename = "n")]
    pub name: String,
    #[serde(rename = "a", default)]
    pub agents: Vec<Agent>,
    #[serde(rename = "s", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub skills: BTreeMap<String, SharedItem>,
    #[serde(rename = "p", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, SharedItem>,
    #[serde(rename = "m", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub servers: BTreeMap<String, SharedServer>,
}

/// A skill or plugin: where it comes from, and the agents it is on for.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SharedItem {
    /// `owner/repo`; empty for a skill that exists only on the sharer's computer.
    #[serde(rename = "r", default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(rename = "f", default, skip_serializing_if = "String::is_empty")]
    pub folder: String,
    #[serde(rename = "c", default, skip_serializing_if = "String::is_empty")]
    pub commit: String,
    /// A plugin's marketplace repository.
    #[serde(rename = "k", default, skip_serializing_if = "String::is_empty")]
    pub marketplace: String,
    #[serde(rename = "o", default)]
    pub on: Vec<Agent>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SharedServer {
    /// Without secret values: whoever imports it types their own.
    #[serde(rename = "x")]
    pub config: ServerConfig,
    #[serde(rename = "o", default)]
    pub on: Vec<Agent>,
}

impl SharedItem {
    pub fn is_local(&self) -> bool {
        self.source.is_empty()
    }

    fn pin(&self) -> Pin {
        Pin {
            source: self.source.clone(),
            folder: self.folder.clone(),
            commit: self.commit.clone(),
            ..Pin::default()
        }
    }
}

fn agents_on(states: Option<&crate::States>) -> Vec<Agent> {
    states
        .into_iter()
        .flatten()
        .filter(|(_, on)| **on)
        .map(|(agent, _)| *agent)
        .collect()
}

impl Shared {
    /// The profile as it can be shared: every item it has on for some agent.
    pub fn from_profile(library: &Library, profile: &Profile) -> Shared {
        let short = |commit: &str| commit.chars().take(SHORT_COMMIT).collect::<String>();
        let mut shared = Shared {
            name: profile.name.clone(),
            agents: profile.agents.clone(),
            ..Shared::default()
        };
        for skill in &library.skills {
            let on = agents_on(profile.skills.get(&skill.name));
            if on.is_empty() {
                continue;
            }
            let item = match library.lock.skills.get(&skill.name) {
                Some(pin) => SharedItem {
                    source: pin.source.clone(),
                    folder: pin.folder.clone(),
                    commit: short(&pin.commit),
                    marketplace: String::new(),
                    on,
                },
                None => SharedItem {
                    on,
                    ..SharedItem::default()
                },
            };
            shared.skills.insert(skill.name.clone(), item);
        }
        for plugin in &library.plugins {
            let on = agents_on(profile.plugins.get(&plugin.id));
            let Some(pin) = library.lock.plugins.get(&plugin.id) else {
                continue;
            };
            if on.is_empty() {
                continue;
            }
            shared.plugins.insert(
                plugin.id.clone(),
                SharedItem {
                    source: pin.pin.source.clone(),
                    folder: pin.pin.folder.clone(),
                    commit: short(&pin.pin.commit),
                    marketplace: pin.marketplace.clone(),
                    on,
                },
            );
        }
        for (name, config) in &library.servers {
            let on = agents_on(profile.servers.get(name));
            if on.is_empty() {
                continue;
            }
            let mut config = config.clone();
            for key in &config.secrets {
                config.env.remove(key);
                config.headers.remove(key);
            }
            shared
                .servers
                .insert(name.clone(), SharedServer { config, on });
        }
        shared
    }

    /// Names of skills that exist only on the sharer's computer, which a code can't carry.
    pub fn local_skills(&self) -> Vec<&str> {
        self.skills
            .iter()
            .filter(|(_, item)| item.is_local())
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Refuses anything that would be unsafe to act on: names that aren't plain folder names,
    /// sources that aren't GitHub repositories, servers with bad names or configs.
    fn check(&self) -> Result<()> {
        for (name, item) in self.skills.iter().chain(&self.plugins) {
            let plain = name.split_once('@').map_or(name.as_str(), |(n, _)| n);
            ensure!(valid_name(plain), "unsafe name in the profile: {name}");
            if !item.is_local() {
                crate::discover::check_source(&item.source)?;
                ensure!(
                    item.commit.len() >= 7 && item.commit.chars().all(|c| c.is_ascii_hexdigit()),
                    "{name} has no usable commit"
                );
                ensure!(
                    item.folder.split('/').all(|p| p != ".." && p != "."),
                    "unsafe folder for {name}"
                );
            }
        }
        for (name, server) in &self.servers {
            ensure!(valid_server_name(name), "unsafe server name: {name}");
            server.config.check()?;
        }
        Ok(())
    }
}

/// The profile as a code to paste: `xolkit1:` and deflated, base64url JSON. Local-only skills
/// are left out.
pub fn encode_code(shared: &Shared) -> String {
    let mut shared = shared.clone();
    shared.skills.retain(|_, item| !item.is_local());
    let json = serde_json::to_vec(&shared).unwrap_or_default();
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    let _ = encoder.write_all(&json);
    let deflated = encoder.finish().unwrap_or_default();
    format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(deflated))
}

/// Reads a code, or a `xolkit://import/…` link holding one.
pub fn decode_code(code: &str) -> Result<Shared> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    let code = code.strip_prefix(LINK_PREFIX).unwrap_or(&code);
    let data = code
        .strip_prefix(PREFIX)
        .context("not a xolkit profile code: it should start with xolkit1:")?;
    let deflated = URL_SAFE_NO_PAD
        .decode(data.trim_end_matches('='))
        .context("the code is damaged")?;
    let mut json = Vec::new();
    flate2::read::DeflateDecoder::new(&deflated[..])
        .take(MAX_JSON)
        .read_to_end(&mut json)
        .context("the code is damaged")?;
    let shared: Shared = serde_json::from_slice(&json).context("the code is damaged")?;
    shared.check()?;
    Ok(shared)
}

/// Writes a `.xolkit` file: the profile, with the files of its local-only skills.
pub fn export_file(library: &Library, shared: &Shared, path: &Path) -> Result<()> {
    let file = fs::File::create(path).with_context(|| format!("could not create {}", path.display()))?;
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
        file,
        flate2::Compression::default(),
    ));
    let json = serde_json::to_vec_pretty(shared)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(json.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "profile.json", &json[..])?;
    for name in shared.local_skills() {
        let skill = library
            .skill(name)
            .with_context(|| format!("{name} is not installed"))?;
        for file in skill.files() {
            tar.append_path_with_name(skill.dir.join(&file), format!("skills/{name}/{file}"))?;
        }
    }
    tar.into_inner()?.finish()?.sync_all()?;
    Ok(())
}

/// Reads a `.xolkit` file. Its local-only skills are unpacked into a staging folder, returned
/// with the profile.
pub fn read_export(env: &Env, path: &Path) -> Result<(Shared, PathBuf)> {
    let size = fs::metadata(path)?.len();
    ensure!(size <= MAX_FILE, "the file is larger than {} MB", MAX_FILE >> 20);
    let dest = env.staging_dir().join(unique());
    fs::create_dir_all(&dest)?;
    // `extract` drops the archive's first folder, as GitHub wraps everything in one.
    let wrapped = wrap_in_folder(path)?;
    let result = extract(&wrapped[..], "", &dest, MAX_FILE).and_then(|_| {
        let json = fs::read(dest.join("profile.json")).context("the file has no profile")?;
        let shared: Shared = serde_json::from_slice(&json).context("the profile is damaged")?;
        shared.check()?;
        for name in shared.local_skills() {
            ensure!(
                dest.join("skills").join(name).join("SKILL.md").is_file(),
                "the file lacks the skill {name}"
            );
        }
        Ok(shared)
    });
    match result {
        Ok(shared) => Ok((shared, dest)),
        Err(e) => {
            let _ = fs::remove_dir_all(&dest);
            Err(e)
        }
    }
}

/// The `.xolkit` archive re-packed under one top folder, the shape `extract` reads.
fn wrap_in_folder(path: &Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut out = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    for entry in archive.entries().context("not a .xolkit file")? {
        let mut entry = entry?;
        let mut header = entry.header().clone();
        let name = format!("x/{}", entry.path()?.display());
        let mut data = Vec::new();
        entry.by_ref().take(MAX_FILE).read_to_end(&mut data)?;
        out.append_data(&mut header, name, &data[..])?;
    }
    Ok(out.into_inner()?.finish()?)
}

/// What importing a shared profile downloads.
#[derive(Clone, Debug, Default)]
pub struct Downloads {
    pub skills: Vec<FetchedSkill>,
    pub plugins: Vec<FetchedPlugin>,
}

impl Downloads {
    pub fn discard(&self) {
        for skill in &self.skills {
            let _ = fs::remove_dir_all(&skill.dir);
        }
        for plugin in &self.plugins {
            plugin.discard();
        }
    }
}

/// Downloads what the profile has that the library lacks, at the commits it pins.
pub fn fetch_import(env: &Env, library: &Library, shared: &Shared, token: Option<&str>) -> Result<Downloads> {
    let mut downloads = Downloads::default();
    let result = (|| {
        for (name, item) in &shared.skills {
            if item.is_local() || library.skill(name).is_some() {
                continue;
            }
            let mut pin = item.pin();
            pin.commit = full_commit(&pin.source, &pin.commit, token)?;
            let mut skill = fetch_skill_at(env, &pin, name, token)?;
            // Install under the name the profile uses, so its switches find the skill.
            skill.name = name.clone();
            downloads.skills.push(skill);
        }
        for (id, item) in &shared.plugins {
            if library.plugin(id).is_some() {
                continue;
            }
            let mut pin = item.pin();
            pin.commit = full_commit(&pin.source, &pin.commit, token)?;
            pin.tree = crate::discover::tree_hash(&pin.source, &pin.commit, &pin.folder, token)?;
            downloads.plugins.push(fetch_plugin_at(
                env,
                id,
                PluginPin {
                    pin,
                    marketplace: item.marketplace.clone(),
                    ..PluginPin::default()
                },
            )?);
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(downloads),
        Err(e) => {
            downloads.discard();
            Err(e)
        }
    }
}

fn full_commit(source: &str, commit: &str, token: Option<&str>) -> Result<String> {
    if commit.len() == 40 {
        return Ok(commit.to_owned());
    }
    let full = crate::discover::github(
        token,
        &format!("https://api.github.com/repos/{source}/commits/{commit}"),
    )
    .header("Accept", "application/vnd.github.sha")
    .call()
    .with_context(|| format!("could not find commit {commit} in github.com/{source}"))?
    .body_mut()
    .read_to_string()?;
    let full = full.trim().to_owned();
    ensure!(full.len() == 40 && full.starts_with(commit), "GitHub returned another commit");
    Ok(full)
}

/// Steps that import a shared profile: install what was downloaded and the local skills from a
/// `.xolkit` file, add servers it has and the library lacks, and add the profile under a free
/// name. Native plugins are not installed here; the app offers that once the profile is used.
pub fn import_steps(
    env: &Env,
    library: &Library,
    shared: &Shared,
    downloads: &Downloads,
    local_dir: Option<&Path>,
) -> Result<Vec<Action>> {
    let mut actions = Vec::new();
    for skill in &downloads.skills {
        actions.extend(crate::install_skill(env, skill)?);
    }
    for name in shared.local_skills() {
        if library.skill(name).is_some() {
            continue;
        }
        let Some(dir) = local_dir else {
            bail!("the profile's skill {name} isn't on this computer, and the code can't carry it");
        };
        actions.push(Action::Copy {
            from: dir.join("skills").join(name),
            to: env.skills_dir().join(name),
        });
    }
    for plugin in &downloads.plugins {
        let target = env.plugins_dir().join(&plugin.plugin.id);
        ensure!(!target.exists(), "{} is installed already", plugin.plugin.id);
        actions.push(Action::Rename {
            from: plugin.plugin.dir.clone(),
            to: target,
        });
        actions.push(Action::PinPlugin {
            id: plugin.plugin.id.clone(),
            pin: Some(plugin.pin.clone()),
        });
    }
    for (name, server) in &shared.servers {
        if !library.servers.contains_key(name) {
            actions.push(Action::SaveServer {
                name: name.clone(),
                config: Some(server.config.clone()),
            });
        }
    }
    let mut settings: Settings = library.settings.clone();
    let mut profile = Profile::new(settings.free_name(&shared.name), shared.agents.clone());
    let lists = [
        (Kind::Skill, &shared.skills),
        (Kind::Plugin, &shared.plugins),
    ];
    for (kind, items) in lists {
        for (name, item) in items {
            for &agent in &item.on {
                profile.set(kind, name, agent, true);
            }
        }
    }
    for (name, server) in &shared.servers {
        for &agent in &server.on {
            profile.set(Kind::Server, name, agent, true);
        }
    }
    settings.profiles.push(profile);
    actions.push(Action::SaveSettings {
        settings: Box::new(settings),
    });
    Ok(actions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::write_skill;
    use crate::{Lock, execute};

    fn library(env: &Env) -> Library {
        write_skill(&env.skills_dir().join("remote"), "remote");
        write_skill(&env.skills_dir().join("mine"), "mine");
        write_skill(&env.skills_dir().join("unused"), "unused");
        let mut lock = Lock::default();
        lock.skills.insert(
            "remote".into(),
            Pin {
                source: "o/r".into(),
                folder: "skills/remote".into(),
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
                ..Pin::default()
            },
        );
        lock.write(&env.lock_file()).unwrap();
        let mut library = Library::load(env).unwrap();
        library.servers.insert(
            "api".into(),
            ServerConfig {
                url: Some("https://x/mcp".into()),
                headers: [("Authorization".into(), "Bearer secret".into())].into(),
                secrets: vec!["Authorization".into()],
                ..ServerConfig::default()
            },
        );
        let profile = library.settings.active_mut();
        profile.name = "Work".into();
        profile.agents = vec![Agent::Claude, Agent::Codex];
        profile.set(Kind::Skill, "remote", Agent::Claude, true);
        profile.set(Kind::Skill, "mine", Agent::Codex, true);
        profile.set(Kind::Skill, "unused", Agent::Codex, false);
        profile.set(Kind::Server, "api", Agent::Claude, true);
        library
    }

    #[test]
    fn codes_round_trip_without_secrets_or_local_skills() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let library = library(&env);
        let shared = Shared::from_profile(&library, library.settings.active());
        assert_eq!(shared.skills.len(), 2);
        assert_eq!(shared.skills["remote"].commit, "0123456789ab");
        assert_eq!(shared.local_skills(), ["mine"]);
        assert!(shared.servers["api"].config.headers.is_empty());

        let code = encode_code(&shared);
        assert!(code.starts_with("xolkit1:"), "{code}");
        assert!(code.len() < 400, "{} characters", code.len());
        let back = decode_code(&format!("  xolkit://import/{code}\n")).unwrap();
        assert_eq!(back.skills.keys().collect::<Vec<_>>(), ["remote"]);
        assert_eq!(back.servers, shared.servers);
        assert!(decode_code("xolkit1:!!!").is_err());
        assert!(decode_code("hello").is_err());

        let mut evil = back.clone();
        evil.skills.get_mut("remote").unwrap().folder = "../..".into();
        assert!(decode_code(&encode_code(&evil)).is_err());
    }

    #[test]
    fn files_carry_local_skills_and_import_as_a_new_profile() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let library = library(&env);
        let shared = Shared::from_profile(&library, library.settings.active());
        let path = home.path().join("work.xolkit");
        export_file(&library, &shared, &path).unwrap();

        let other = tempfile::tempdir().unwrap();
        let other_env = Env::at(other.path());
        let (read, staged) = read_export(&other_env, &path).unwrap();
        assert_eq!(read, shared);
        let empty = Library::load(&other_env).unwrap();
        // The remote skill would be downloaded; here only the local one is installed.
        let mut without_remote = read.clone();
        without_remote.skills.remove("remote");
        let actions =
            import_steps(&other_env, &empty, &without_remote, &Downloads::default(), Some(&staged))
                .unwrap();
        execute(&other_env, &actions).unwrap();
        let imported = Library::load(&other_env).unwrap();
        assert!(imported.skill("mine").is_some());
        assert!(imported.servers.contains_key("api"));
        let profile = &imported.settings.profiles[1];
        assert_eq!(profile.name, "Work");
        assert!(profile.switched_on(Kind::Skill, "mine", Agent::Codex));
        assert!(profile.switched_on(Kind::Server, "api", Agent::Claude));

        // A code can't bring the local skill.
        assert!(import_steps(&other_env, &empty, &shared, &Downloads::default(), None).is_err());
    }
}
