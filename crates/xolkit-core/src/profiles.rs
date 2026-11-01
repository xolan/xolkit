//! Profiles: named sets of the agents xolkit serves, and which skills, plugins and MCP servers
//! are on for each of them. The daemon serves what the active profile has on; switching
//! profiles changes what every open session sees.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Agent;

/// On or off, per agent. An agent missing here has the item off.
pub type States = BTreeMap<Agent, bool>;

/// What a switch turns on or off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Skill,
    Plugin,
    Server,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    /// The agents this profile serves, in display order.
    #[serde(default)]
    pub agents: Vec<Agent>,
    /// By skill folder name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub skills: BTreeMap<String, States>,
    /// By plugin id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, States>,
    /// By server name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub servers: BTreeMap<String, States>,
}

impl Profile {
    pub fn new(name: impl Into<String>, agents: Vec<Agent>) -> Profile {
        Profile {
            name: name.into(),
            agents,
            ..Profile::default()
        }
    }

    fn items(&self, kind: Kind) -> &BTreeMap<String, States> {
        match kind {
            Kind::Skill => &self.skills,
            Kind::Plugin => &self.plugins,
            Kind::Server => &self.servers,
        }
    }

    fn items_mut(&mut self, kind: Kind) -> &mut BTreeMap<String, States> {
        match kind {
            Kind::Skill => &mut self.skills,
            Kind::Plugin => &mut self.plugins,
            Kind::Server => &mut self.servers,
        }
    }

    pub fn serves(&self, agent: Agent) -> bool {
        self.agents.contains(&agent)
    }

    /// Whether the item is switched on for `agent`, whether or not the profile serves the agent.
    pub fn switched_on(&self, kind: Kind, name: &str, agent: Agent) -> bool {
        self.items(kind)
            .get(name)
            .and_then(|states| states.get(&agent))
            .copied()
            .unwrap_or(false)
    }

    /// Whether `agent` gets the item: the profile serves the agent and has the item on for it.
    pub fn is_on(&self, kind: Kind, name: &str, agent: Agent) -> bool {
        self.serves(agent) && self.switched_on(kind, name, agent)
    }

    pub fn set(&mut self, kind: Kind, name: &str, agent: Agent, on: bool) {
        self.items_mut(kind)
            .entry(name.to_owned())
            .or_default()
            .insert(agent, on);
    }

    /// Drops an item that is no longer installed.
    pub fn forget(&mut self, kind: Kind, name: &str) {
        self.items_mut(kind).remove(name);
    }

    /// Turns serving `agent` on or off, keeping the agents in display order.
    pub fn set_agent(&mut self, agent: Agent, on: bool) {
        self.agents = Agent::ALL
            .into_iter()
            .filter(|&a| if a == agent { on } else { self.agents.contains(&a) })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_are_off_until_turned_on_for_a_served_agent() {
        let mut profile = Profile::new("Work", vec![Agent::Claude]);
        assert!(!profile.is_on(Kind::Skill, "s", Agent::Claude));
        profile.set(Kind::Skill, "s", Agent::Claude, true);
        profile.set(Kind::Skill, "s", Agent::Codex, true);
        assert!(profile.is_on(Kind::Skill, "s", Agent::Claude));
        // Codex isn't served, so it gets nothing, but the switch is kept for when it is.
        assert!(!profile.is_on(Kind::Skill, "s", Agent::Codex));
        assert!(profile.switched_on(Kind::Skill, "s", Agent::Codex));
        assert!(!profile.is_on(Kind::Server, "s", Agent::Claude));
        profile.forget(Kind::Skill, "s");
        assert!(!profile.switched_on(Kind::Skill, "s", Agent::Claude));
    }

    #[test]
    fn agents_stay_in_display_order() {
        let mut profile = Profile::new("p", vec![]);
        profile.set_agent(Agent::Other, true);
        profile.set_agent(Agent::Claude, true);
        assert_eq!(profile.agents, [Agent::Claude, Agent::Other]);
        profile.set_agent(Agent::Claude, false);
        assert_eq!(profile.agents, [Agent::Other]);
    }

    #[test]
    fn saves_agents_by_name() {
        let mut profile = Profile::new("p", vec![Agent::Codex]);
        profile.set(Kind::Plugin, "p@m", Agent::Other, true);
        let json = serde_json::to_value(&profile).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"name": "p", "agents": ["codex"], "plugins": {"p@m": {"other": true}}})
        );
        assert_eq!(serde_json::from_value::<Profile>(json).unwrap(), profile);
    }
}
