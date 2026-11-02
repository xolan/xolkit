//! Skills in the library: one folder each under `~/.xolkit/skills`, or inside a plugin.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// A skill xolkit can serve.
#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    /// Directory name; the skill's identity.
    pub name: String,
    /// `name` from the SKILL.md frontmatter.
    pub title: String,
    pub description: String,
    /// The folder holding SKILL.md and the skill's other files.
    pub dir: PathBuf,
}

impl Skill {
    pub fn skill_md(&self) -> PathBuf {
        self.dir.join("SKILL.md")
    }

    /// Reads the skill in `dir`, named after the folder. `None` without a SKILL.md.
    pub fn read(dir: &Path) -> Option<Skill> {
        let name = dir.file_name()?.to_str()?.to_owned();
        let text = fs::read_to_string(dir.join("SKILL.md")).ok()?;
        let front = frontmatter(&text);
        Some(Skill {
            title: front.name.unwrap_or_else(|| name.clone()),
            description: front.description.unwrap_or_default(),
            name,
            dir: dir.to_path_buf(),
        })
    }

    /// SKILL.md without its frontmatter.
    pub fn body(&self) -> String {
        let text = fs::read_to_string(self.skill_md()).unwrap_or_default();
        strip_frontmatter(&text)
    }

    /// Every file in the skill's folder, relative to it, sorted.
    pub fn files(&self) -> Vec<String> {
        let mut files = Vec::new();
        list_files(&self.dir, "", &mut files);
        files.sort();
        files
    }
}

fn list_files(dir: &Path, prefix: &str, files: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => list_files(&entry.path(), &rel, files),
            Ok(kind) if kind.is_file() => files.push(rel),
            _ => {}
        }
    }
}

/// The skills in `root`: visible subdirectories that hold a SKILL.md, sorted by name.
pub fn scan_skills(root: &Path) -> Vec<Skill> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut skills: Vec<Skill> = entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        // Following links, so skills kept in a dotfiles repository count.
        .filter(|e| e.path().is_dir())
        .filter_map(|e| Skill::read(&e.path()))
        .collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct Frontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
}

/// `name` and `description` from a SKILL.md. Falls back to a line scan, because many skills
/// ship frontmatter that is not valid YAML (e.g. unquoted colons in the description).
pub(crate) fn frontmatter(text: &str) -> Frontmatter {
    // Windows PowerShell 5.1 and some Windows editors start UTF-8 files with a byte order mark.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text.strip_prefix("---") else {
        return Frontmatter::default();
    };
    let block = rest.split("\n---").next().unwrap_or_default();
    if let Ok(front) = serde_yaml_ng::from_str::<Frontmatter>(block) {
        return front;
    }
    let field = |key: &str| {
        block.lines().find_map(|line| {
            let value = line.strip_prefix(key)?.strip_prefix(':')?.trim();
            Some(value.trim_matches(['"', '\'']).to_owned())
        })
    };
    Frontmatter {
        name: field("name"),
        description: field("description"),
    }
}

/// A Markdown file without its frontmatter.
pub fn strip_frontmatter(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    text.strip_prefix("---")
        .and_then(|rest| rest.split_once("\n---"))
        .map(|(_, body)| body.trim_start_matches(['-', '\n', '\r']).to_owned())
        .unwrap_or_else(|| text.to_owned())
}

#[cfg(test)]
pub(crate) fn write_skill(dir: &Path, name: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Does {name}.\n---\n\nBody of {name}\n"),
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_parses_yaml_and_falls_back() {
        assert_eq!(
            frontmatter("---\nname: a\ndescription: >\n  folded\n  text\n---\nbody"),
            Frontmatter {
                name: Some("a".into()),
                description: Some("folded text".into())
            }
        );
        assert_eq!(
            frontmatter("---\nname: b\ndescription: Use when: things\n---\n"),
            Frontmatter {
                name: Some("b".into()),
                description: Some("Use when: things".into())
            }
        );
        assert_eq!(frontmatter("no frontmatter"), Frontmatter::default());
    }

    #[test]
    fn frontmatter_after_a_byte_order_mark_and_crlf() {
        let expected = Frontmatter {
            name: Some("a".into()),
            description: Some("Does a: well.".into()),
        };
        for text in [
            "\u{feff}---\nname: a\ndescription: \"Does a: well.\"\n---\n",
            "---\r\nname: a\r\ndescription: \"Does a: well.\"\r\n---\r\nbody\r\n",
            "\u{feff}---\r\nname: a\r\ndescription: Does a: well.\r\n---\r\n",
        ] {
            assert_eq!(frontmatter(text), expected, "{text:?}");
        }
    }

    #[test]
    fn scans_skill_folders_only() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(&dir.path().join("b"), "b");
        write_skill(&dir.path().join("a"), "renamed");
        write_skill(&dir.path().join(".hidden"), "hidden");
        fs::create_dir(dir.path().join("empty")).unwrap();
        fs::write(dir.path().join("loose.md"), "x").unwrap();
        let skills = scan_skills(dir.path());
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(skills[0].title, "renamed");
        assert_eq!(skills[1].description, "Does b.");
        assert_eq!(skills[1].body(), "Body of b\n");
        assert_eq!(skills[1].files(), ["SKILL.md"]);
        assert!(scan_skills(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn strips_frontmatter() {
        assert_eq!(strip_frontmatter("---\nname: a\n---\n\nBody"), "Body");
        assert_eq!(strip_frontmatter("Plain"), "Plain");
    }
}
