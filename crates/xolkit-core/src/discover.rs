//! Finding new skills on skills.sh and fetching them from GitHub.

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::lock::{Lock, Pin};
use crate::skills::frontmatter;
use crate::{Action, Env};

const USER_AGENT: &str = concat!("xolkit/", env!("CARGO_PKG_VERSION"));
/// Largest repository archive to download.
const MAX_ARCHIVE: u64 = 200 * 1024 * 1024;
/// Largest skill folder to extract.
const MAX_SKILL: u64 = 20 * 1024 * 1024;
/// Largest plugin folder to extract.
pub(crate) const MAX_PLUGIN: u64 = 100 * 1024 * 1024;

/// A skills.sh search result.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SkillHit {
    /// `owner/repo/skill`.
    pub id: String,
    /// The GitHub repository, `owner/repo`.
    pub source: String,
    pub skill_id: String,
    pub name: String,
    #[serde(default)]
    pub installs: u64,
}

pub fn search_skills(query: &str) -> Result<Vec<SkillHit>> {
    #[derive(Deserialize)]
    struct Response {
        skills: Vec<SkillHit>,
    }
    let request = || {
        ureq::get("https://skills.sh/api/search")
            .header("User-Agent", USER_AGENT)
            .query("q", query)
            .query("limit", "50")
            .call()
    };
    // skills.sh answers 503 now and then; one retry clears it.
    let response = match request() {
        Err(ureq::Error::StatusCode(code)) if code >= 500 => {
            std::thread::sleep(std::time::Duration::from_millis(500));
            request()
        }
        other => other,
    };
    let response: Response = response
        .context("skills.sh search failed")?
        .body_mut()
        .read_json()
        .context("unexpected skills.sh response")?;
    Ok(response.skills)
}

/// A skill downloaded into a staging directory, ready to install.
#[derive(Clone, Debug)]
pub struct FetchedSkill {
    /// Directory name it installs as.
    pub name: String,
    /// The staging directory holding the skill's files.
    pub dir: PathBuf,
    /// Where it came from, at the commit downloaded.
    pub pin: Pin,
    pub skill_md: String,
    /// Every file, relative to the skill folder.
    pub files: Vec<String>,
}

impl FetchedSkill {
    /// Removes the staging directory.
    pub fn discard(&self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Downloads `skill_id` from the GitHub repository `source` into a staging directory. `token`
/// raises GitHub's API rate limit from 60 to 5,000 requests an hour.
pub fn fetch_skill(
    env: &Env,
    source: &str,
    skill_id: &str,
    token: Option<&str>,
) -> Result<FetchedSkill> {
    let commit = head_commit(source, token)?;
    let tree: Tree = serde_json::from_str(
        &github(
            token,
            &format!("https://api.github.com/repos/{source}/git/trees/{commit}?recursive=1"),
        )
        .call()?
        .body_mut()
        .with_config()
        .limit(MAX_ARCHIVE)
        .read_to_string()?,
    )
    .context("unexpected GitHub tree response")?;
    let folder = find_skill_folder(&tree, skill_id, |path| {
        let url = format!("https://raw.githubusercontent.com/{source}/{commit}/{path}");
        Ok(github(token, &url).call()?.body_mut().read_to_string()?)
    })?;
    let folder_hash = if folder.is_empty() {
        tree.sha.clone()
    } else {
        tree.tree
            .iter()
            .find(|e| e.kind == "tree" && e.path == folder)
            .map(|e| e.sha.clone())
            .context("skill folder missing from the GitHub tree")?
    };
    download_skill(env, source, &commit, &folder, folder_hash, skill_id)
}

/// Downloads the skill in `folder` of `source` at `commit`, as a shared profile pins it.
pub fn fetch_skill_at(
    env: &Env,
    pin: &Pin,
    name: &str,
    token: Option<&str>,
) -> Result<FetchedSkill> {
    ensure!(
        pin.commit.len() >= 7 && pin.commit.chars().all(|c| c.is_ascii_hexdigit()),
        "not a commit: {}",
        pin.commit
    );
    let folder_hash = tree_hash(&pin.source, &pin.commit, &pin.folder, token)?;
    download_skill(
        env,
        &pin.source,
        &pin.commit,
        &pin.folder,
        folder_hash,
        name,
    )
}

/// Extracts `folder` of `source` at `commit` into a new staging directory, returning it and the
/// files in it. Nothing is left behind when it fails.
pub(crate) fn download_folder(
    env: &Env,
    source: &str,
    commit: &str,
    folder: &str,
    max: u64,
) -> Result<(PathBuf, Vec<String>)> {
    check_source(source)?;
    let dir = env.staging_dir().join(unique());
    fs::create_dir_all(&dir)?;
    let archive = github(
        None,
        &format!("https://codeload.github.com/{source}/tar.gz/{commit}"),
    )
    .call()
    .context("could not download the repository")?
    .into_body()
    .into_with_config()
    .limit(MAX_ARCHIVE)
    .reader();
    match extract(archive, folder, &dir, max) {
        Ok(files) => Ok((dir, files)),
        Err(e) => {
            let _ = fs::remove_dir_all(&dir);
            Err(e)
        }
    }
}

/// Downloads a skill folder into a staging directory. The skill is named by its frontmatter,
/// else `fallback_name`.
fn download_skill(
    env: &Env,
    source: &str,
    commit: &str,
    folder: &str,
    folder_hash: String,
    fallback_name: &str,
) -> Result<FetchedSkill> {
    let (dir, files) = download_folder(env, source, commit, folder, MAX_SKILL)?;
    if !files.iter().any(|f| f == "SKILL.md") {
        let _ = fs::remove_dir_all(&dir);
        bail!("the download has no SKILL.md");
    }
    let skill_md = fs::read_to_string(dir.join("SKILL.md")).unwrap_or_default();
    let name = frontmatter(&skill_md)
        .name
        .filter(|n| valid_name(n))
        .unwrap_or_else(|| fallback_name.to_owned());
    if !valid_name(&name) {
        let _ = fs::remove_dir_all(&dir);
        bail!("unsafe skill name: {name}");
    }
    Ok(FetchedSkill {
        name,
        dir,
        pin: Pin {
            source: source.to_owned(),
            folder: folder.to_owned(),
            commit: commit.to_owned(),
            tree: folder_hash,
            ..Pin::default()
        },
        skill_md,
        files,
    })
}

/// Moves a fetched skill into `~/.xolkit/skills` and records where it came from. Fails if the
/// name is already taken.
pub fn install_skill(env: &Env, skill: &FetchedSkill) -> Result<Vec<Action>> {
    ensure!(valid_name(&skill.name), "unsafe skill name: {}", skill.name);
    let target = env.skills_dir().join(&skill.name);
    ensure!(
        fs::symlink_metadata(&target).is_err(),
        "{} already exists",
        env.tilde(&target)
    );
    let now = now();
    Ok(vec![
        Action::Rename {
            from: skill.dir.clone(),
            to: target,
        },
        Action::PinSkill {
            name: skill.name.clone(),
            pin: Some(Pin {
                installed_at: now.clone(),
                updated_at: now,
                ..skill.pin.clone()
            }),
        },
    ])
}

/// Copies a skill folder from disk into `~/.xolkit/skills`, named like the folder. Fails if
/// the folder has no SKILL.md, holds links, or the name is taken.
pub fn add_local_skill(env: &Env, dir: &Path) -> Result<(String, Vec<Action>)> {
    ensure!(
        dir.join("SKILL.md").is_file(),
        "{} has no SKILL.md",
        env.tilde(dir)
    );
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| valid_name(n))
        .with_context(|| format!("{} can't be a skill name", dir.display()))?
        .to_owned();
    let target = env.skills_dir().join(&name);
    ensure!(
        fs::symlink_metadata(&target).is_err(),
        "a skill named {name} is installed already"
    );
    Ok((
        name,
        vec![Action::Copy {
            from: dir.to_path_buf(),
            to: target,
        }],
    ))
}

fn now() -> String {
    humantime::format_rfc3339_millis(SystemTime::now()).to_string()
}

/// A newer version of an installed skill on GitHub.
#[derive(Clone, Debug, PartialEq)]
pub struct SkillUpdate {
    /// Directory name of the installed skill.
    pub name: String,
    pub source: String,
    /// The skill's folder in the repository, `""` for the root.
    pub folder: String,
    /// Git tree hash of the folder on GitHub now.
    pub folder_hash: String,
}

/// Compares the folder hash recorded at install with the folder on GitHub now. `None` when they
/// match, or the skill was not installed from GitHub.
pub fn check_skill_update(
    env: &Env,
    name: &str,
    token: Option<&str>,
) -> Result<Option<SkillUpdate>> {
    let lock = Lock::read(&env.lock_file())?;
    let Some(pin) = lock.skills.get(name).filter(|p| !p.tree.is_empty()) else {
        return Ok(None);
    };
    let folder_hash = tree_hash(&pin.source, "HEAD", &pin.folder, token)?;
    Ok((folder_hash != pin.tree).then(|| SkillUpdate {
        name: name.to_owned(),
        source: pin.source.clone(),
        folder: pin.folder.clone(),
        folder_hash,
    }))
}

/// Downloads the newer version of an installed skill into a staging directory.
pub fn fetch_update(env: &Env, update: &SkillUpdate, token: Option<&str>) -> Result<FetchedSkill> {
    let commit = head_commit(&update.source, token)?;
    let folder_hash = tree_hash(&update.source, &commit, &update.folder, token)?;
    download_skill(
        env,
        &update.source,
        &commit,
        &update.folder,
        folder_hash,
        &update.name,
    )
}

/// Swaps installed skill `name` for a downloaded newer version and records the new commit. The
/// old copy is moved aside first and deleted last, so a failed step never leaves a half-deleted
/// skill.
pub fn update_skill(env: &Env, name: &str, skill: &FetchedSkill) -> Result<Vec<Action>> {
    let target = env.skills_dir().join(name);
    ensure!(
        fs::symlink_metadata(&target).is_ok_and(|m| m.is_dir()),
        "{} is not a folder xolkit can replace",
        env.tilde(&target)
    );
    let installed_at = Lock::read(&env.lock_file())?
        .skills
        .get(name)
        .map(|p| p.installed_at.clone())
        .unwrap_or_default();
    let old = env.staging_dir().join(format!("{}-old", unique()));
    Ok(vec![
        Action::Rename {
            from: target.clone(),
            to: old.clone(),
        },
        Action::Rename {
            from: skill.dir.clone(),
            to: target,
        },
        Action::PinSkill {
            name: name.to_owned(),
            pin: Some(Pin {
                installed_at,
                updated_at: now(),
                ..skill.pin.clone()
            }),
        },
        Action::Remove { path: old },
    ])
}

/// Removes downloads left by an earlier run: those over an hour old, so another running copy of
/// the app keeps the ones it is showing.
pub fn clean_staging(env: &Env) {
    let Ok(entries) = fs::read_dir(env.staging_dir()) else {
        return;
    };
    let hour = std::time::Duration::from_secs(3600);
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > hour));
        if old {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

#[derive(Deserialize)]
struct Tree {
    sha: String,
    tree: Vec<TreeEntry>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Deserialize)]
struct TreeEntry {
    path: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
}

/// The folder (`""` for the repository root) holding the skill: a SKILL.md folder named
/// `skill_id`, else the only one there is, else the one whose frontmatter names it.
fn find_skill_folder(
    tree: &Tree,
    skill_id: &str,
    read: impl Fn(&str) -> Result<String>,
) -> Result<String> {
    let folders: Vec<&str> = tree
        .tree
        .iter()
        .filter(|e| e.kind == "blob")
        .filter_map(|e| {
            if e.path == "SKILL.md" {
                Some("")
            } else {
                e.path.strip_suffix("/SKILL.md")
            }
        })
        .collect();
    let by_dir: Vec<&str> = folders
        .iter()
        .copied()
        .filter(|f| f.rsplit('/').next() == Some(skill_id))
        .collect();
    if let [folder] = by_dir[..] {
        return Ok(folder.to_owned());
    }
    if let [folder] = folders[..] {
        return Ok(folder.to_owned());
    }
    let candidates = if by_dir.is_empty() { &folders } else { &by_dir };
    for folder in candidates.iter().take(50) {
        let path = if folder.is_empty() {
            "SKILL.md".to_owned()
        } else {
            format!("{folder}/SKILL.md")
        };
        if frontmatter(&read(&path)?).name.as_deref() == Some(skill_id) {
            return Ok((*folder).to_owned());
        }
    }
    if tree.truncated {
        bail!("the repository is too large to search for {skill_id}");
    }
    bail!("no skill named {skill_id} in the repository")
}

/// Extracts the files under `folder` from a GitHub tarball into `dest`. Refuses links and any
/// path that would leave `dest`.
pub(crate) fn extract(
    archive: impl Read,
    folder: &str,
    dest: &Path,
    max: u64,
) -> Result<Vec<String>> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    let folder: Vec<String> = folder
        .split('/')
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect();
    let mut files = Vec::new();
    let mut total = 0;
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        // GitHub wraps everything in one `<repo>-<commit>/` directory.
        let parts: Vec<String> = path
            .components()
            .skip(1)
            .map(|c| match c {
                Component::Normal(part) => Ok(part.to_string_lossy().into_owned()),
                _ => bail!("unsafe path in archive: {}", path.display()),
            })
            .collect::<Result<_>>()?;
        let Some(rel) = parts.strip_prefix(&folder[..]) else {
            continue;
        };
        if rel.is_empty() {
            continue;
        }
        ensure!(
            rel.iter().all(|part| storable(part)),
            "the download has a file name Windows can't store: {}",
            rel.join("/")
        );
        let out = rel.iter().fold(dest.to_path_buf(), |p, part| p.join(part));
        match entry.header().entry_type() {
            tar::EntryType::Directory => fs::create_dir_all(&out)?,
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                total += entry.size();
                ensure!(total <= max, "the download is larger than {} MB", max >> 20);
                fs::create_dir_all(out.parent().context("no parent")?)?;
                entry.unpack(&out)?;
                files.push(rel.join("/"));
            }
            tar::EntryType::Symlink | tar::EntryType::Link => {
                bail!(
                    "the download contains a link ({}); refusing to install it",
                    rel.join("/")
                )
            }
            _ => {}
        }
    }
    ensure!(!files.is_empty(), "the download is empty");
    files.sort();
    Ok(files)
}

/// Whether Windows can store a file named `name` as it is; elsewhere every name is fine. Windows
/// would fail on the others halfway through, or worse: `C:` or `name:stream` lead out of the
/// folder or into a hidden stream, and a trailing dot or space is dropped, so `SKILL.md.` would
/// overwrite `SKILL.md`.
fn storable(name: &str) -> bool {
    const DEVICES: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM0", "COM1", "COM2", "COM3", "COM4",
        "COM5", "COM6", "COM7", "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT0", "LPT1", "LPT2",
        "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³",
    ];
    if !cfg!(windows) {
        return true;
    }
    // Device names are taken with any extension too (`nul.txt`) on Windows 10.
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    !name.ends_with(['.', ' '])
        && !name.chars().any(|c| c < ' ' || r#"<>:"/\|?*"#.contains(c))
        && !DEVICES
            .iter()
            .any(|device| stem.eq_ignore_ascii_case(device))
}

/// A name that is safe as one folder name and as a command-line argument: letters, digits, `-`,
/// `_` and `.`, not starting with `.` or `-`.
pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

/// `name@marketplace`, both safe names.
pub(crate) fn valid_plugin_id(id: &str) -> bool {
    id.split_once('@')
        .is_some_and(|(name, marketplace)| valid_name(name) && valid_name(marketplace))
}

/// Refuses anything but `owner/repo`, since it goes into GitHub URLs.
pub(crate) fn check_source(source: &str) -> Result<()> {
    ensure!(
        source.split('/').count() == 2 && source.split('/').all(valid_name),
        "not a GitHub repository: {source}"
    );
    Ok(())
}

/// The commit `HEAD` of a GitHub repository points to.
pub(crate) fn head_commit(source: &str, token: Option<&str>) -> Result<String> {
    check_source(source)?;
    let commit = github(
        token,
        &format!("https://api.github.com/repos/{source}/commits/HEAD"),
    )
    .header("Accept", "application/vnd.github.sha")
    .call()
    .with_context(|| format!("could not reach github.com/{source}"))?
    .body_mut()
    .read_to_string()?
    .trim()
    .to_owned();
    ensure!(
        commit.chars().all(|c| c.is_ascii_hexdigit()),
        "unexpected commit id from GitHub"
    );
    Ok(commit)
}

/// Git tree hash of `folder` (`""` for the root) at `rev`, one small API call.
pub(crate) fn tree_hash(
    source: &str,
    rev: &str,
    folder: &str,
    token: Option<&str>,
) -> Result<String> {
    #[derive(Deserialize)]
    struct Tree {
        sha: String,
    }
    check_source(source)?;
    ensure!(
        folder.is_empty()
            || folder
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != ".." && !p.contains(['?', '#', '%'])),
        "unsafe skill folder: {folder}"
    );
    let rev = if folder.is_empty() {
        rev.to_owned()
    } else {
        format!("{rev}:{folder}")
    };
    let tree: Tree = github(
        token,
        &format!("https://api.github.com/repos/{source}/git/trees/{rev}"),
    )
    .call()
    .with_context(|| format!("could not read {folder} in github.com/{source}"))?
    .body_mut()
    .read_json()
    .context("unexpected GitHub tree response")?;
    Ok(tree.sha)
}

pub(crate) fn github(
    token: Option<&str>,
    url: &str,
) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
    let request = ureq::get(url).header("User-Agent", USER_AGENT);
    // Only the API needs the token; never send it anywhere else.
    match token {
        Some(token) if url.starts_with("https://api.github.com/") => {
            request.header("Authorization", format!("Bearer {token}"))
        }
        _ => request,
    }
}

/// The token the GitHub CLI is signed in with (`gh auth token`).
pub fn gh_token(env: &Env) -> Result<String> {
    let token = env.run("gh", &["auth".into(), "token".into()])?;
    let token = token.trim();
    ensure!(!token.is_empty(), "gh is not signed in");
    Ok(token.to_owned())
}

/// GitHub's core API rate limit for `token` (or for anonymous use): `(limit, remaining)` per hour.
/// Checking it does not count against the limit.
pub fn github_rate_limit(token: Option<&str>) -> Result<(u64, u64)> {
    #[derive(Deserialize)]
    struct Core {
        limit: u64,
        remaining: u64,
    }
    #[derive(Deserialize)]
    struct Resources {
        core: Core,
    }
    #[derive(Deserialize)]
    struct Response {
        resources: Resources,
    }
    let response: Response = match github(token, "https://api.github.com/rate_limit").call() {
        Err(ureq::Error::StatusCode(401)) => {
            bail!("GitHub rejected the token: it is invalid or has expired")
        }
        result => result.context("could not reach GitHub")?,
    }
    .body_mut()
    .read_json()
    .context("unexpected GitHub response")?;
    Ok((
        response.resources.core.limit,
        response.resources.core.remaining,
    ))
}

pub(crate) fn unique() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(paths: &[&str]) -> Tree {
        Tree {
            sha: "root".into(),
            tree: paths
                .iter()
                .map(|p| TreeEntry {
                    path: (*p).into(),
                    kind: "blob".into(),
                    sha: "x".into(),
                })
                .collect(),
            truncated: false,
        }
    }

    #[test]
    fn finds_skill_folder() {
        let no_read = |_: &str| -> Result<String> { panic!("should not read") };
        let t = tree(&["skills/a/SKILL.md", "skills/b/SKILL.md", "README.md"]);
        assert_eq!(find_skill_folder(&t, "b", no_read).unwrap(), "skills/b");
        assert_eq!(
            find_skill_folder(&tree(&["SKILL.md"]), "x", no_read).unwrap(),
            ""
        );
        let by_name = |path: &str| -> Result<String> {
            Ok(if path == "skills/b/SKILL.md" {
                "---\nname: renamed\n---\n"
            } else {
                ""
            }
            .into())
        };
        assert_eq!(
            find_skill_folder(&t, "renamed", by_name).unwrap(),
            "skills/b"
        );
        assert!(find_skill_folder(&t, "missing", |_| Ok(String::new())).is_err());
    }

    fn tarball(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (path, kind, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            if *kind == tar::EntryType::Symlink {
                header.set_link_name("../../etc/passwd").unwrap();
            }
            // Bypass the builder's path checks so unsafe paths reach the extractor.
            let name = &mut header.as_old_mut().name;
            name[..path.len()].copy_from_slice(path.as_bytes());
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn extracts_only_the_skill_folder() {
        let dest = tempfile::tempdir().unwrap();
        let archive = tarball(&[
            ("repo-abc/README.md", tar::EntryType::Regular, b"x"),
            (
                "repo-abc/skills/a/SKILL.md",
                tar::EntryType::Regular,
                b"---\nname: a\n---\n",
            ),
            (
                "repo-abc/skills/a/scripts/run.sh",
                tar::EntryType::Regular,
                b"echo",
            ),
            (
                "repo-abc/skills/ab/SKILL.md",
                tar::EntryType::Regular,
                b"other",
            ),
        ]);
        let files = extract(&archive[..], "skills/a", dest.path(), MAX_SKILL).unwrap();
        assert_eq!(files, ["SKILL.md", "scripts/run.sh"]);
        assert!(dest.path().join("scripts/run.sh").is_file());
        assert!(!dest.path().join("README.md").exists());
    }

    #[test]
    fn refuses_links_and_traversal() {
        let dest = tempfile::tempdir().unwrap();
        let link = tarball(&[
            ("repo-abc/s/SKILL.md", tar::EntryType::Regular, b"x"),
            ("repo-abc/s/evil", tar::EntryType::Symlink, b""),
        ]);
        assert!(extract(&link[..], "s", dest.path(), MAX_SKILL).is_err());
        let traversal = tarball(&[("repo-abc/s/../../evil", tar::EntryType::Regular, b"x")]);
        assert!(extract(&traversal[..], "s", dest.path(), MAX_SKILL).is_err());
        assert!(!dest.path().parent().unwrap().join("evil").exists());
    }

    #[test]
    fn refuses_names_windows_cannot_store_on_windows() {
        for name in [
            "C:/evil",
            "SKILL.md:hidden",
            "CON",
            "nul.txt",
            "a?b",
            "trail.",
            "trail ",
        ] {
            let dest = tempfile::tempdir().unwrap();
            let archive = tarball(&[
                ("repo-abc/s/SKILL.md", tar::EntryType::Regular, b"x"),
                (&format!("repo-abc/s/{name}"), tar::EntryType::Regular, b"x"),
            ]);
            let result = extract(&archive[..], "s", dest.path(), MAX_SKILL);
            assert_eq!(result.is_err(), cfg!(windows), "{name}");
        }
        // `C:evil` would be relative to the current directory on drive C.
        assert!(!Path::new("C:evil").exists());
        // Names elsewhere in the repository don't matter.
        let dest = tempfile::tempdir().unwrap();
        let archive = tarball(&[
            ("repo-abc/s/SKILL.md", tar::EntryType::Regular, b"x"),
            ("repo-abc/other/CON", tar::EntryType::Regular, b"x"),
        ]);
        assert!(extract(&archive[..], "s", dest.path(), MAX_SKILL).is_ok());
    }

    fn fetched(env: &Env, name: &str, text: &str) -> FetchedSkill {
        let dir = env.staging_dir().join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), text).unwrap();
        FetchedSkill {
            name: name.into(),
            dir,
            pin: Pin {
                source: "o/r".into(),
                folder: format!("skills/{name}"),
                commit: "c0ffee".into(),
                tree: "new".into(),
                ..Pin::default()
            },
            skill_md: text.into(),
            files: vec!["SKILL.md".into()],
        }
    }

    #[test]
    fn install_moves_and_pins() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let skill = fetched(&env, "s", "x");
        crate::execute(&env, &install_skill(&env, &skill).unwrap()).unwrap();
        assert!(env.skills_dir().join("s/SKILL.md").is_file());
        let pin = &Lock::read(&env.lock_file()).unwrap().skills["s"];
        assert_eq!((pin.commit.as_str(), pin.tree.as_str()), ("c0ffee", "new"));
        assert!(!pin.installed_at.is_empty());
        assert!(install_skill(&env, &fetched(&env, "s", "y")).is_err());
    }

    #[test]
    fn adds_a_local_folder() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let dir = home.path().join("mine/tidy");
        crate::skills::write_skill(&dir, "tidy");
        let (name, actions) = add_local_skill(&env, &dir).unwrap();
        assert_eq!(name, "tidy");
        crate::execute(&env, &actions).unwrap();
        assert!(env.skills_dir().join("tidy/SKILL.md").is_file());
        assert!(add_local_skill(&env, &dir).is_err());
        assert!(add_local_skill(&env, home.path()).is_err());
    }

    #[test]
    fn update_swaps_the_folder() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let target = env.skills_dir().join("s");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("SKILL.md"), "old").unwrap();
        fs::write(target.join("gone.md"), "x").unwrap();
        Lock::edit(&env.lock_file(), |lock| {
            lock.skills.insert(
                "s".into(),
                Pin {
                    installed_at: "then".into(),
                    ..Pin::default()
                },
            );
        })
        .unwrap();
        let skill = fetched(&env, "renamed", "new");
        crate::execute(&env, &update_skill(&env, "s", &skill).unwrap()).unwrap();
        assert_eq!(fs::read_to_string(target.join("SKILL.md")).unwrap(), "new");
        assert!(!target.join("gone.md").exists());
        assert_eq!(fs::read_dir(env.staging_dir()).unwrap().count(), 0);
        let pin = &Lock::read(&env.lock_file()).unwrap().skills["s"];
        assert_eq!(pin.tree, "new");
        assert_eq!(pin.installed_at, "then");
        assert!(!pin.updated_at.is_empty());
    }

    #[test]
    fn update_never_replaces_a_link() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let dotfiles = home.path().join("dotfiles/s");
        fs::create_dir_all(&dotfiles).unwrap();
        fs::create_dir_all(env.skills_dir()).unwrap();
        crate::testing::link_dir(&dotfiles, env.skills_dir().join("s"));
        let skill = fetched(&env, "s", "new");
        assert!(update_skill(&env, "s", &skill).is_err());
    }

    /// Finds an outdated skill on GitHub and downloads its update into a throwaway home.
    #[test]
    #[ignore = "uses the network"]
    fn check_and_fetch_update() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        Lock::edit(&env.lock_file(), |lock| {
            lock.skills.insert(
                "find-skills".into(),
                Pin {
                    source: "vercel-labs/skills".into(),
                    folder: "skills/find-skills".into(),
                    commit: "0000".into(),
                    tree: "0000".into(),
                    ..Pin::default()
                },
            );
        })
        .unwrap();
        let update = check_skill_update(&env, "find-skills", None)
            .unwrap()
            .unwrap();
        assert_eq!(update.folder, "skills/find-skills");
        assert_eq!(update.folder_hash.len(), 40);
        let fetched = fetch_update(&env, &update, None).unwrap();
        assert_eq!(fetched.pin.tree, update.folder_hash);
        assert!(fetched.files.contains(&"SKILL.md".to_owned()));
    }

    /// Searches skills.sh and installs a real skill into a throwaway home.
    #[test]
    #[ignore = "uses the network"]
    fn search_fetch_and_install() {
        let hit = search_skills("find-skills")
            .unwrap()
            .into_iter()
            .find(|h| h.source == "vercel-labs/skills" && h.skill_id == "find-skills")
            .expect("skills.sh lists vercel-labs/skills find-skills");
        let home = tempfile::tempdir().unwrap();
        let env = Env::at(home.path());
        let fetched = fetch_skill(&env, &hit.source, &hit.skill_id, None).unwrap();
        assert_eq!(fetched.name, "find-skills");
        assert_eq!(fetched.pin.tree.len(), 40);
        assert!(fetched.files.contains(&"SKILL.md".to_owned()));
        crate::execute(&env, &install_skill(&env, &fetched).unwrap()).unwrap();
        let skills = crate::skills::scan_skills(&env.skills_dir());
        assert_eq!(skills[0].name, "find-skills");
        let again = fetch_skill_at(&env, &fetched.pin, "find-skills", None).unwrap();
        assert_eq!(again.pin.tree, fetched.pin.tree);
    }

    #[test]
    #[ignore = "uses the network"]
    fn anonymous_rate_limit() {
        let (limit, remaining) = github_rate_limit(None).unwrap();
        assert_eq!(limit, 60);
        assert!(remaining <= limit);
    }
}
