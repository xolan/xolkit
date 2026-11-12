//! Updating xolkit itself from its GitHub releases.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::discover::github;

const REPO: &str = "xolan/xolkit";
/// Largest release archive to download.
const MAX_ARCHIVE: u64 = 200 * 1024 * 1024;
/// How long a check, and a whole download, may take before it fails and can be tried again.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// A release newer than the running build, with its download for this platform.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    /// The release's page on GitHub.
    pub url: String,
    /// The archive for this platform and its `.sha256` file; `None` when the release has none.
    download: Option<(String, String)>,
}

impl Release {
    pub fn has_download(&self) -> bool {
        self.download.is_some()
    }
}

#[derive(Deserialize)]
struct Latest {
    tag_name: String,
    html_url: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// The latest release on GitHub, if it is newer than this build.
pub fn check_for_update() -> Result<Option<Release>> {
    let latest: Latest = get(
        &format!("https://api.github.com/repos/{REPO}/releases/latest"),
        CHECK_TIMEOUT,
    )
    .context("could not reach GitHub")?
    .body_mut()
    .read_json()
    .context("unexpected GitHub release response")?;
    newer_release(latest, env!("CARGO_PKG_VERSION"))
}

fn newer_release(latest: Latest, current: &str) -> Result<Option<Release>> {
    let version = latest.tag_name.trim_start_matches('v');
    let parse = |v: &str| semver::Version::parse(v).with_context(|| format!("bad version {v}"));
    if parse(version)? <= parse(current)? {
        return Ok(None);
    }
    let archive = archive_name(version);
    let url = |name: &str| {
        latest
            .assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.browser_download_url.clone())
    };
    let download = url(&archive).zip(url(&format!("{archive}.sha256")));
    Ok(Some(Release {
        version: version.to_owned(),
        url: latest.html_url,
        download,
    }))
}

/// The release archive for this platform, as `mise run package` names it.
fn archive_name(version: &str) -> String {
    let arch = std::env::consts::ARCH;
    if cfg!(windows) {
        format!("xolkit-{version}-{arch}-pc-windows-msvc.zip")
    } else if cfg!(target_os = "macos") {
        format!("xolkit-{version}-{arch}-apple-darwin.tar.gz")
    } else {
        format!("xolkit-{version}-{arch}-unknown-linux-gnu.tar.gz")
    }
}

/// Linux packages install under `/usr` and must update through their package manager.
/// macOS installs are standalone binaries, including when the user chooses `/usr/local/bin`.
pub fn installed_by_package_manager(exe: &Path) -> bool {
    cfg!(target_os = "linux") && exe.starts_with("/usr")
}

/// Downloads the release, checks it against its checksum, and puts its binary in place of `exe`.
/// The running program keeps running the old one until it restarts.
pub fn install_update(release: &Release, exe: &Path) -> Result<()> {
    ensure!(
        !installed_by_package_manager(exe),
        "xolkit was installed by a package manager; update it there"
    );
    let (archive_url, checksum_url) = release
        .download
        .as_ref()
        .context("the release has no download for this computer")?;
    let checksum = get(checksum_url, CHECK_TIMEOUT)
        .context("could not download the checksum")?
        .body_mut()
        .read_to_string()?;
    let archive = get(archive_url, DOWNLOAD_TIMEOUT)
        .context("could not download the release")?
        .body_mut()
        .with_config()
        .limit(MAX_ARCHIVE)
        .read_to_vec()?;
    verify(&archive, &checksum)?;
    let binary = extract_binary(&archive, &release.version)?;
    replace_exe(exe, &binary)
}

/// An anonymous GET that fails once `timeout` passes, reading the body included.
fn get(url: &str, timeout: Duration) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    github(None, url)
        .config()
        .timeout_global(Some(timeout))
        .build()
        .call()
}

/// Checks `archive` against a `sha256sum` line.
fn verify(archive: &[u8], checksum: &str) -> Result<()> {
    let expected = checksum.split_whitespace().next().unwrap_or_default();
    ensure!(
        expected.len() == 64 && expected.chars().all(|c| c.is_ascii_hexdigit()),
        "the release's checksum file is malformed"
    );
    let actual: String = Sha256::digest(archive)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    ensure!(
        actual.eq_ignore_ascii_case(expected),
        "the download doesn't match the release's checksum"
    );
    Ok(())
}

/// The binary inside a Linux or macOS tarball: `xolkit-<version>-<target>/xolkit`.
#[cfg(not(windows))]
fn extract_binary(archive: &[u8], version: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    let folder = archive_name(version);
    let wanted = Path::new(folder.trim_end_matches(".tar.gz")).join("xolkit");
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_file() && *entry.path()? == *wanted {
            let mut binary = Vec::new();
            entry.read_to_end(&mut binary)?;
            return Ok(binary);
        }
    }
    anyhow::bail!("the release has no {}", wanted.display())
}

/// The binary inside a Windows zip, unpacked with Windows' own `tar`, which reads zip files.
#[cfg(windows)]
fn extract_binary(archive: &[u8], version: &str) -> Result<Vec<u8>> {
    let folder = archive_name(version);
    let folder = folder.trim_end_matches(".zip");
    let dir = std::env::temp_dir().join(format!("xolkit-update-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;
    let result = (|| {
        let zip = dir.join("release.zip");
        fs::write(&zip, archive)?;
        let system = std::env::var_os("SYSTEMROOT").context("SYSTEMROOT is not set")?;
        let status = std::process::Command::new(Path::new(&system).join(r"System32\tar.exe"))
            .arg("-xf")
            .arg(&zip)
            .arg("-C")
            .arg(&dir)
            .arg(format!("{folder}/xolkit.exe"))
            .status()
            .context("could not run tar to unpack the release")?;
        ensure!(status.success(), "could not unpack the release");
        Ok(fs::read(dir.join(folder).join("xolkit.exe"))?)
    })();
    let _ = fs::remove_dir_all(&dir);
    result
}

/// `exe` with `suffix` added to its file name.
fn beside(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_owned();
    name.push(suffix);
    exe.with_file_name(name)
}

/// Where a replaced binary waits on Windows, which can't delete a program that is running.
fn old_exe(exe: &Path) -> PathBuf {
    beside(exe, ".old")
}

/// Writes `binary` next to `exe`, then renames it over `exe`. Linux keeps running the old file,
/// which stays on disk until the program exits; Windows can rename, but not overwrite, a running
/// program, so it is moved aside first.
fn replace_exe(exe: &Path, binary: &[u8]) -> Result<()> {
    let new = beside(exe, ".new");
    let result = put_in_place(exe, &new, binary);
    // Gone already when the rename worked.
    let _ = fs::remove_file(&new);
    result
}

fn put_in_place(exe: &Path, new: &Path, binary: &[u8]) -> Result<()> {
    fs::write(new, binary).with_context(|| format!("could not write next to {}", exe.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(new, fs::Permissions::from_mode(0o755))?;
    }
    if cfg!(windows) {
        let old = old_exe(exe);
        let _ = fs::remove_file(&old);
        fs::rename(exe, &old).context("could not move the running program aside")?;
        if let Err(e) = fs::rename(new, exe) {
            let _ = fs::rename(&old, exe);
            return Err(e).context("could not put the new version in place");
        }
    } else {
        fs::rename(new, exe).context("could not put the new version in place")?;
    }
    Ok(())
}

/// Removes the binary an earlier update moved aside on Windows. The restart starts this program
/// before the old one quits, so it tries again for a few seconds. Blocks while it waits.
pub fn clean_old_exe(exe: &Path) {
    if !cfg!(windows) {
        return;
    }
    let old = old_exe(exe);
    for _ in 0..20 {
        match fs::remove_file(&old) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                std::thread::sleep(Duration::from_millis(500))
            }
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latest(tag: &str, assets: &[&str]) -> Latest {
        Latest {
            tag_name: tag.into(),
            html_url: format!("https://github.com/{REPO}/releases/tag/{tag}"),
            assets: assets
                .iter()
                .map(|name| Asset {
                    name: (*name).into(),
                    browser_download_url: format!("https://example.com/{name}"),
                })
                .collect(),
        }
    }

    #[test]
    fn only_newer_releases_count() {
        let archive = archive_name("2.1.0");
        let sum = format!("{archive}.sha256");
        let assets = [archive.as_str(), sum.as_str()];
        assert_eq!(
            newer_release(latest("v2.0.0", &assets), "2.0.0").unwrap(),
            None
        );
        assert_eq!(
            newer_release(latest("v1.9.9", &assets), "2.0.0").unwrap(),
            None
        );
        let release = newer_release(latest("v2.1.0", &assets), "2.0.0")
            .unwrap()
            .unwrap();
        assert_eq!(release.version, "2.1.0");
        assert_eq!(
            release.download,
            Some((
                format!("https://example.com/{archive}"),
                format!("https://example.com/{sum}")
            ))
        );
        // Without its checksum the archive isn't offered.
        let bare = newer_release(latest("v2.1.0", &[&archive]), "2.0.0")
            .unwrap()
            .unwrap();
        assert!(!bare.has_download());
        assert!(newer_release(latest("nightly", &[]), "2.0.0").is_err());
    }

    #[test]
    fn checksum_must_match() {
        let sum = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        verify(b"hello", &format!("{sum}  xolkit.tar.gz\n")).unwrap();
        assert!(verify(b"hellO", sum).is_err());
        assert!(verify(b"hello", "").is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn extracts_only_the_expected_binary() {
        use flate2::{Compression, write::GzEncoder};
        let pack = |path: &str| {
            let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
            let mut header = tar::Header::new_gnu();
            header.set_size(6);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, path, &b"binary"[..]).unwrap();
            tar.into_inner().unwrap().finish().unwrap()
        };
        let folder = archive_name("9.0.0");
        let folder = folder.trim_end_matches(".tar.gz");
        assert_eq!(
            extract_binary(&pack(&format!("{folder}/xolkit")), "9.0.0").unwrap(),
            b"binary"
        );
        assert!(extract_binary(&pack("elsewhere/xolkit"), "9.0.0").is_err());
    }

    #[test]
    fn replaces_the_program() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join(if cfg!(windows) {
            "xolkit.exe"
        } else {
            "xolkit"
        });
        fs::write(&exe, "old").unwrap();
        replace_exe(&exe, b"new").unwrap();
        assert_eq!(fs::read_to_string(&exe).unwrap(), "new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&exe).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        clean_old_exe(&exe);
        let mut left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        left.sort();
        assert_eq!(left, [exe.file_name().unwrap()]);
    }

    #[test]
    fn package_installs_are_left_alone() {
        assert_eq!(
            installed_by_package_manager(Path::new("/usr/bin/xolkit")),
            cfg!(target_os = "linux")
        );
        assert_eq!(
            installed_by_package_manager(Path::new("/usr/local/bin/xolkit")),
            cfg!(target_os = "linux")
        );
        assert!(!installed_by_package_manager(Path::new(
            "/home/me/.local/bin/xolkit"
        )));
    }

    /// Installs the latest release over a stand-in program, as an update to it would.
    #[test]
    #[ignore = "uses the network"]
    fn installs_the_latest_release() {
        let latest: Latest = github(
            None,
            &format!("https://api.github.com/repos/{REPO}/releases/latest"),
        )
        .call()
        .unwrap()
        .body_mut()
        .read_json()
        .unwrap();
        let release = newer_release(latest, "0.0.1").unwrap().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join(if cfg!(windows) {
            "xolkit.exe"
        } else {
            "xolkit"
        });
        fs::write(&exe, "old").unwrap();
        install_update(&release, &exe).unwrap();
        let binary = fs::read(&exe).unwrap();
        let magic: &[u8] = if cfg!(windows) {
            b"MZ"
        } else if cfg!(target_os = "macos") {
            b"\xcf\xfa\xed\xfe" // 64-bit little-endian Mach-O (Intel and Apple Silicon).
        } else {
            b"\x7fELF"
        };
        assert!(binary.starts_with(magic));
    }
}
