//! What one agent gets from xolkit under the active profile: skills, prompts and MCP servers.

use crate::{Agent, Command, Kind, Library, Server, Skill};

/// The longest catalogue of skills put in the instructions an agent reads when it connects.
/// Past it, the rest are listed by name only, and past twice that, left to `find_skill`.
const INSTRUCTIONS_BUDGET: usize = 6000;
/// The skills spec's limit on a skill's description.
const DESCRIPTION_LIMIT: usize = 1024;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Catalogue {
    /// Skills of the library and of plugins, sorted by name. A plugin's skill is named
    /// `<plugin>:<skill>`.
    pub skills: Vec<Skill>,
    /// Plugin commands, named `<plugin>:<command>`.
    pub commands: Vec<Command>,
    pub servers: Vec<Server>,
}

/// An item of the catalogue, for the router skill's digest and for search.
pub enum Item<'a> {
    Skill(&'a Skill),
    Command(&'a Command),
}

/// What `agent` gets: what the active profile has on for it, without the parts of plugins it
/// has installed natively, which it reads from its own folders.
pub fn catalogue(library: &Library, agent: Agent) -> Catalogue {
    let profile = library.settings.active();
    let mut catalogue = Catalogue::default();
    for skill in &library.skills {
        if profile.is_on(Kind::Skill, &skill.name, agent) {
            catalogue.skills.push(skill.clone());
        }
    }
    let all_servers = library.all_servers();
    for server in &all_servers {
        if server.plugin.is_none() && profile.is_on(Kind::Server, &server.name, agent) {
            catalogue.servers.push(server.clone());
        }
    }
    for plugin in &library.plugins {
        let native = agent
            .provider()
            .is_some_and(|p| library.lock.native(&plugin.id).contains(&p));
        if native || !profile.is_on(Kind::Plugin, &plugin.id, agent) {
            continue;
        }
        for skill in &plugin.parts.skills {
            catalogue.skills.push(Skill {
                name: format!("{}:{}", plugin.name, skill.name),
                ..skill.clone()
            });
        }
        for command in &plugin.parts.commands {
            catalogue.commands.push(Command {
                name: format!("{}:{}", plugin.name, command.name),
                ..command.clone()
            });
        }
        for server in &all_servers {
            if server.plugin.as_deref() == Some(&plugin.id) {
                catalogue.servers.push(server.clone());
            }
        }
    }
    catalogue.skills.sort_by(|a, b| a.name.cmp(&b.name));
    catalogue.commands.sort_by(|a, b| a.name.cmp(&b.name));
    catalogue
}

/// The first sentence of `text`, on one line.
fn first_sentence(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.find(". ") {
        Some(end) => text[..=end].to_owned(),
        None => text,
    }
}

impl Catalogue {
    pub fn skill(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    pub fn command(&self, name: &str) -> Option<&Command> {
        self.commands.iter().find(|c| c.name == name)
    }

    /// The instructions an agent reads when it connects: how to use xolkit, and the skills it
    /// has, as many as fit.
    pub fn instructions(&self) -> String {
        let mut text = String::from(
            "xolkit serves skills: instructions for specific tasks. When a request matches a \
             skill below, call `load_skill` with its name before you start, and follow what it \
             says. Its files are in the folder `load_skill` returns; read or run them from \
             there. If no skill below fits, `find_skill` searches them by topic.\n",
        );
        if self.skills.is_empty() {
            text.push_str("\nNo skills are on for you right now.\n");
            return text;
        }
        text.push_str("\nSkills:\n");
        let mut names_only = Vec::new();
        for skill in &self.skills {
            let line = format!("- {}: {}\n", skill.name, first_sentence(&skill.description));
            if text.len() + line.len() <= INSTRUCTIONS_BUDGET {
                text.push_str(&line);
            } else {
                names_only.push(skill.name.as_str());
            }
        }
        if !names_only.is_empty() {
            let mut listed = 0;
            let mut more = String::new();
            for name in &names_only {
                if text.len() + more.len() + name.len() + 2 > INSTRUCTIONS_BUDGET * 2 {
                    break;
                }
                if listed > 0 {
                    more.push_str(", ");
                }
                more.push_str(name);
                listed += 1;
            }
            text.push_str(&format!("\nAlso: {more}"));
            let rest = names_only.len() - listed;
            if rest > 0 {
                text.push_str(&format!(", and {rest} more `find_skill` can find"));
            }
            text.push('\n');
        }
        text
    }

    /// Skills and commands matching `query`, best first: every word of the query counts, more
    /// in a name than in a description.
    pub fn find(&self, query: &str) -> Vec<Item<'_>> {
        let words: Vec<String> = query
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 1)
            .map(str::to_owned)
            .collect();
        let score = |name: &str, description: &str| -> usize {
            let name = name.to_lowercase();
            let description = description.to_lowercase();
            words
                .iter()
                .map(|w| 3 * usize::from(name.contains(w.as_str())) + usize::from(description.contains(w.as_str())))
                .sum()
        };
        let mut found: Vec<(usize, Item<'_>)> = self
            .skills
            .iter()
            .map(|s| (score(&s.name, &format!("{} {}", s.title, s.description)), Item::Skill(s)))
            .chain(
                self.commands
                    .iter()
                    .map(|c| (score(&c.name, &c.description), Item::Command(c))),
            )
            .filter(|(score, _)| *score > 0 || words.is_empty())
            .collect();
        found.sort_by(|a, b| b.0.cmp(&a.0));
        found.into_iter().map(|(_, item)| item).collect()
    }
}

/// The router skill's SKILL.md for an agent: one skill whose description is a digest of the
/// trigger terms of everything the agent has on, so the agent finds xolkit's skills even when
/// it ignores the MCP server's instructions.
pub fn router_skill(catalogue: &Catalogue) -> String {
    let intro = "Skills xolkit serves over MCP. Use when a task involves: ";
    let mut digest = String::new();
    for skill in &catalogue.skills {
        let term = if skill.description.is_empty() {
            skill.name.clone()
        } else {
            let sentence = first_sentence(&skill.description);
            let short: String = sentence.chars().take(80).collect();
            format!("{} ({})", skill.name, short.trim_end_matches('.'))
        };
        let separator = if digest.is_empty() { "" } else { "; " };
        if intro.len() + digest.len() + separator.len() + term.len() + 1 > DESCRIPTION_LIMIT {
            break;
        }
        digest.push_str(separator);
        digest.push_str(&term);
    }
    if digest.is_empty() {
        digest.push_str("nothing yet");
    }
    let description = format!("{intro}{digest}.").replace(['"', '\n'], " ");
    format!(
        "---\nname: xolkit\ndescription: \"{description}\"\n---\n\n\
         # xolkit\n\n\
         This skill is written by xolkit; changes to it are overwritten.\n\n\
         xolkit serves your skills through its MCP server, `xolkit`. To use one:\n\n\
         1. Call the `find_skill` tool with a few words about the task, or with the skill's name.\n\
         2. Call `load_skill` with the name it returns, and follow the instructions you get.\n\
         3. Read or run the skill's other files from the folder `load_skill` gives.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::write_plugin;
    use crate::skills::write_skill;
    use crate::{Env, PluginPin, Provider};

    fn library(home: &std::path::Path) -> Library {
        let env = Env::at(home);
        write_skill(&env.skills_dir().join("deploy"), "deploy");
        write_skill(&env.skills_dir().join("tidy"), "tidy");
        write_plugin(&env.plugins_dir(), "p@m", false);
        let mut library = Library::load(&env).unwrap();
        let profile = library.settings.active_mut();
        profile.agents = Agent::ALL.to_vec();
        profile.set(Kind::Skill, "deploy", Agent::Claude, true);
        profile.set(Kind::Skill, "tidy", Agent::Codex, true);
        profile.set(Kind::Plugin, "p@m", Agent::Claude, true);
        profile.set(Kind::Plugin, "p@m", Agent::Codex, true);
        library
    }

    #[test]
    fn each_agent_gets_what_is_on_for_it() {
        let home = tempfile::tempdir().unwrap();
        let mut library = library(home.path());
        let claude = catalogue(&library, Agent::Claude);
        let names: Vec<_> = claude.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["deploy", "p:helper"]);
        let commands: Vec<_> = claude.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(commands, ["p:git:commit", "p:review"]);
        assert_eq!(claude.servers.len(), 1);
        assert!(catalogue(&library, Agent::Other).skills.is_empty());

        // Codex has the plugin natively, so xolkit serves it only its own skill.
        library.lock.plugins.insert(
            "p@m".into(),
            PluginPin {
                native: [Provider::Codex].into(),
                ..PluginPin::default()
            },
        );
        let codex = catalogue(&library, Agent::Codex);
        let names: Vec<_> = codex.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["tidy"]);
        assert!(codex.servers.is_empty() && codex.commands.is_empty());

        // An agent the profile doesn't serve gets nothing.
        library.settings.active_mut().agents = vec![Agent::Codex];
        assert!(catalogue(&library, Agent::Claude).skills.is_empty());
    }

    #[test]
    fn instructions_list_skills_within_the_budget() {
        let home = tempfile::tempdir().unwrap();
        let claude = catalogue(&library(home.path()), Agent::Claude);
        let text = claude.instructions();
        assert!(text.contains("- deploy: Does deploy.\n"), "{text}");
        let many = Catalogue {
            skills: (0..1000)
                .map(|i| Skill {
                    name: format!("skill-{i:03}"),
                    title: String::new(),
                    description: "A fairly long description of what this skill does. More.".into(),
                    dir: Default::default(),
                })
                .collect(),
            ..Catalogue::default()
        };
        let text = many.instructions();
        assert!(text.len() <= INSTRUCTIONS_BUDGET * 2 + 100, "{}", text.len());
        assert!(text.contains("Also: "));
        assert!(text.contains("more `find_skill` can find"));
        assert!(Catalogue::default().instructions().contains("No skills"));
    }

    #[test]
    fn finds_by_name_before_description() {
        let home = tempfile::tempdir().unwrap();
        let claude = catalogue(&library(home.path()), Agent::Claude);
        let found = claude.find("review deploy");
        let names: Vec<_> = found
            .iter()
            .map(|item| match item {
                Item::Skill(s) => s.name.as_str(),
                Item::Command(c) => c.name.as_str(),
            })
            .collect();
        assert_eq!(names, ["deploy", "p:review"]);
        assert!(claude.find("zzz").is_empty());
    }

    #[test]
    fn router_skill_digest_fits_the_limit() {
        let home = tempfile::tempdir().unwrap();
        let claude = catalogue(&library(home.path()), Agent::Claude);
        let text = router_skill(&claude);
        let front = crate::skills::frontmatter(&text);
        assert_eq!(front.name.as_deref(), Some("xolkit"));
        let description = front.description.unwrap();
        assert!(description.contains("deploy (Does deploy)"), "{description}");
        let many = Catalogue {
            skills: (0..200)
                .map(|i| Skill {
                    name: format!("s{i}"),
                    title: String::new(),
                    description: "Words \"quoted\" here.".into(),
                    dir: Default::default(),
                })
                .collect(),
            ..Catalogue::default()
        };
        let description = crate::skills::frontmatter(&router_skill(&many))
            .description
            .unwrap();
        assert!(description.len() <= DESCRIPTION_LIMIT, "{}", description.len());
    }
}
