//! `~/.xolkit/lock.json`: where each skill and plugin came from, pinned to a commit, and which
//! agents have a plugin installed natively.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::Provider;
use crate::config::{read_json, write_json};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Lock {
    #[serde(default)]
    pub skills: BTreeMap<String, Pin>,
    #[serde(default)]
    pub plugins: BTreeMap<String, PluginPin>,
}

/// A folder of a GitHub repository at a commit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pin {
    /// `owner/repo`.
    pub source: String,
    /// The folder in the repository, `""` for its root.
    #[serde(default)]
    pub folder: String,
    pub commit: String,
    /// Git tree hash of the folder at `commit`; a newer one on GitHub means an update.
    #[serde(default)]
    pub tree: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub installed_at: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated_at: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginPin {
    #[serde(flatten)]
    pub pin: Pin,
    /// The marketplace repository it was listed in, `owner/repo`.
    #[serde(default)]
    pub marketplace: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    /// Agents that installed it from xolkit's marketplace.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub native: BTreeSet<Provider>,
}

impl Lock {
    pub fn read(path: &Path) -> Result<Lock> {
        let json = read_json(path)?;
        serde_json::from_value(json).with_context(|| format!("{} is not valid", path.display()))
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let mut json = serde_json::to_value(self)?;
        json["version"] = 1.into();
        write_json(path, &json)
    }

    /// Reads the lock file, edits it and writes it back.
    pub(crate) fn edit(path: &Path, edit: impl FnOnce(&mut Lock)) -> Result<()> {
        let mut lock = Lock::read(path)?;
        let before = lock.clone();
        edit(&mut lock);
        if lock != before {
            lock.write(path)?;
        }
        Ok(())
    }

    /// The agents that have plugin `id` installed natively.
    pub fn native(&self, id: &str) -> BTreeSet<Provider> {
        self.plugins
            .get(id)
            .map(|p| p.native.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_tolerates_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock.json");
        assert_eq!(Lock::read(&path).unwrap(), Lock::default());
        let pin = Pin {
            source: "o/r".into(),
            folder: "skills/a".into(),
            commit: "abc".into(),
            tree: "def".into(),
            ..Pin::default()
        };
        Lock::edit(&path, |lock| {
            lock.skills.insert("a".into(), pin.clone());
            lock.plugins.insert(
                "p@m".into(),
                PluginPin {
                    pin: pin.clone(),
                    marketplace: "o/m".into(),
                    native: BTreeSet::from([Provider::Claude]),
                    ..PluginPin::default()
                },
            );
        })
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\": 1"), "{text}");
        assert!(text.contains("\"native\": [\n        \"claude\""), "{text}");
        let lock = Lock::read(&path).unwrap();
        assert_eq!(lock.skills["a"], pin);
        assert_eq!(lock.native("p@m"), BTreeSet::from([Provider::Claude]));
        assert!(lock.native("other").is_empty());
    }
}
