//! One agent session: an MCP server that offers the agent what its catalogue holds. Skills come
//! as the `find_skill` and `load_skill` tools and as prompts, plugin commands as prompts, and
//! the tools of MCP servers as `<server>__<tool>`.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, GetPromptRequestParams,
    GetPromptResponse, GetPromptResult, Implementation, JsonObject, ListPromptsResult,
    ListToolsResult, PaginatedRequestParams, Prompt, PromptArgument, PromptMessage, Role,
    ServerCapabilities, ServerConfig, SubscriptionFilter, Tool,
};
use rmcp::service::{RequestContext, SubscriptionContext};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::json;
use xolkit_core::{Agent, Catalogue, Item};

use crate::daemon::Daemon;

/// Separates a server's name from its tool's in the names agents see.
pub(crate) const SEPARATOR: &str = "__";

#[derive(Clone)]
pub(crate) struct Session {
    pub(crate) daemon: Arc<Daemon>,
    pub(crate) id: u64,
    pub(crate) agent: Agent,
}

/// A prompt name agents accept: letters, digits, `-` and `_`.
fn prompt_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The prompts of a catalogue, each under a name of its own: skills first, then commands. Two
/// names that read alike once made prompt-safe (`p:review` and `p-review`) get `-2` and on, so
/// the listing and the lookup always agree.
fn prompts(catalogue: &Catalogue) -> Vec<(String, Item<'_>)> {
    let mut taken = std::collections::HashSet::new();
    let items = catalogue
        .skills
        .iter()
        .map(|s| (s.name.as_str(), Item::Skill(s)))
        .chain(
            catalogue
                .commands
                .iter()
                .map(|c| (c.name.as_str(), Item::Command(c))),
        );
    items
        .map(|(name, item)| {
            let base = prompt_name(name);
            let name = (1..)
                .map(|n| {
                    if n == 1 {
                        base.clone()
                    } else {
                        format!("{base}-{n}")
                    }
                })
                .find(|candidate| !taken.contains(candidate))
                .unwrap_or(base);
            taken.insert(name.clone());
            (name, item)
        })
        .collect()
}

/// The longest name agents accept for a tool of xolkit's: tool names are limited to 64
/// characters, and Claude Code names it `mcp__xolkit__<name>`.
const MAX_TOOL_NAME: usize = 64 - "mcp__xolkit__".len();

/// The fewest characters of a tool's own name kept when shortening it.
const MIN_KEPT: usize = 8;

/// Why a server's tool is left out of what agents see.
#[derive(Debug, PartialEq)]
pub(crate) enum LeftOut {
    /// Agents would not accept its name, however it was shortened.
    Unusable,
    /// Another tool of the server has the name agents would see for it.
    Taken,
}

impl std::fmt::Display for LeftOut {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            LeftOut::Unusable => "agents would not accept its name",
            LeftOut::Taken => "another tool has the name agents would see for it",
        })
    }
}

/// The names agents see for the `tools` of `server`, in their order. No two tools get one name,
/// and a name that needed no shortening goes to its tool before any shortened to the same, so
/// the names of tools that fit never change or go to another tool.
pub(crate) fn tool_names<'a>(
    server: &str,
    tools: impl IntoIterator<Item = &'a str>,
) -> Vec<Result<String, LeftOut>> {
    let full = |tool: &str| server.len() + SEPARATOR.len() + tool.len();
    let tools: Vec<&str> = tools.into_iter().collect();
    let mut names: Vec<_> = tools
        .iter()
        .map(|tool| tool_name(server, tool).ok_or(LeftOut::Unusable))
        .collect();
    let mut taken = std::collections::HashSet::new();
    for shortened in [false, true] {
        for (tool, name) in tools.iter().zip(&mut names) {
            if let Ok(seen) = name
                && (seen.len() < full(tool)) == shortened
                && !taken.insert(seen.clone())
            {
                *name = Err(LeftOut::Taken);
            }
        }
    }
    names
}

/// The server's own name for the tool agents see as `seen`, among its `tools`.
pub(crate) fn own_tool_name<'a>(
    server: &str,
    tools: impl IntoIterator<Item = &'a str> + Clone,
    seen: &str,
) -> Option<&'a str> {
    tools
        .clone()
        .into_iter()
        .zip(tool_names(server, tools))
        .find_map(|(tool, name)| (name.as_deref() == Ok(seen)).then_some(tool))
}

/// The name agents see for `tool` of `server`: `<server>__<tool>`, or `None` if agents would
/// not accept it, as tool names are letters, digits, `_` and `-`. One that would be too long is
/// shortened: first by dropping the server's name from the front of the tool's
/// (`planetscale__planetscale_list_x` to `planetscale__list_x`), then by cutting the end and
/// closing with a few characters that tell it from other tools cut at the same place.
fn tool_name(server: &str, tool: &str) -> Option<String> {
    let usable = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    if !usable(server) || !usable(tool) {
        return None;
    }
    let room = MAX_TOOL_NAME.checked_sub(server.len() + SEPARATOR.len())?;
    if tool.len() <= room {
        return Some(format!("{server}{SEPARATOR}{tool}"));
    }
    // Only a whole word: `git` is not dropped from `github_create_issue`.
    let short = tool
        .strip_prefix(server)
        .filter(|rest| rest.starts_with(['_', '-']))
        .map(|rest| rest.trim_start_matches(['_', '-']))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(tool);
    if short.len() <= room {
        return Some(format!("{server}{SEPARATOR}{short}"));
    }
    // FNV-1a, so a tool keeps its name from one version of xolkit to the next.
    let hash = tool.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    let kept = room.checked_sub(5).filter(|kept| *kept >= MIN_KEPT)?;
    let kept = short[..kept].trim_end_matches(['_', '-']);
    (!kept.is_empty()).then(|| format!("{server}{SEPARATOR}{kept}_{:04x}", hash & 0xffff))
}

fn schema(value: serde_json::Value) -> Arc<JsonObject> {
    Arc::new(value.as_object().cloned().unwrap_or_default())
}

fn text(text: impl Into<String>) -> CallToolResponse {
    CallToolResult::success(vec![ContentBlock::text(text)]).into()
}

fn error_result(text: impl Into<String>) -> CallToolResponse {
    CallToolResult::error(vec![ContentBlock::text(text)]).into()
}

impl Session {
    /// Records the client's name and version once a request carries them: the current protocol
    /// sends them with every request, older ones once at the start.
    fn note_client(&self, context: &RequestContext<RoleServer>) {
        if let Some(info) = context.client_info() {
            self.daemon
                .session_client(self.id, format!("{} {}", info.name, info.version));
        }
    }

    fn catalogue(&self) -> Catalogue {
        self.daemon.catalogue(self.agent)
    }

    fn own_tools() -> Vec<Tool> {
        vec![
            Tool::new(
                "find_skill",
                "Searches the skills xolkit serves you by topic or name, best match first. Use \
                 it when no skill in xolkit's instructions fits the task.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "A few words about the task, or a skill's name."}
                    },
                    "required": ["query"]
                })),
            ),
            Tool::new(
                "load_skill",
                "Loads a skill by name: its instructions, and the folder holding its other \
                 files. Follow the instructions for the task at hand.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "The skill's name, as listed."}
                    },
                    "required": ["name"]
                })),
            ),
        ]
    }

    fn find_skill(&self, query: &str) -> CallToolResponse {
        let catalogue = self.catalogue();
        let found = catalogue.find(query);
        if found.is_empty() {
            return text(format!("No skill matches “{query}”."));
        }
        let names = prompts(&catalogue);
        let lines: Vec<String> = found
            .iter()
            .take(15)
            .map(|item| match item {
                Item::Skill(s) => format!("- {} (skill): {}", s.name, s.description),
                Item::Command(c) => {
                    let prompt = names
                        .iter()
                        .find(|(_, i)| matches!(i, Item::Command(other) if other.name == c.name))
                        .map_or("", |(name, _)| name.as_str());
                    format!(
                        "- {} (prompt, run it as /mcp__xolkit__{prompt}): {}",
                        c.name, c.description
                    )
                }
            })
            .collect();
        text(format!(
            "{}\n\nLoad a skill with `load_skill` before using it.",
            lines.join("\n")
        ))
    }

    fn load_skill(&self, name: &str) -> CallToolResponse {
        let catalogue = self.catalogue();
        let Some(skill) = catalogue.skill(name) else {
            return error_result(format!(
                "No skill named “{name}” is on for you. `find_skill` lists the ones that are."
            ));
        };
        let files: Vec<String> = skill
            .files()
            .into_iter()
            .filter(|f| f != "SKILL.md")
            .collect();
        let mut out = format!(
            "# Skill: {}\n\nFolder: {}\n",
            skill.name,
            skill.dir.display()
        );
        if !files.is_empty() {
            out.push_str(&format!(
                "Other files, relative to that folder: {}\n",
                files.join(", ")
            ));
        }
        out.push_str("\n---\n\n");
        out.push_str(&skill.body());
        text(out)
    }
}

impl ServerHandler for Session {
    fn get_info(&self) -> ServerConfig {
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .enable_prompts()
            .enable_prompts_list_changed()
            .build();
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new("xolkit", env!("CARGO_PKG_VERSION")))
            .with_instructions(self.catalogue().instructions())
    }

    async fn on_initialized(&self, context: rmcp::service::NotificationContext<RoleServer>) {
        if let Some(info) = context.peer.peer_info() {
            self.daemon.session_client(
                self.id,
                format!("{} {}", info.client_info.name, info.client_info.version),
            );
        }
        // Older protocol versions: tell the agent when its lists change, until it goes away.
        let mut changes = self.daemon.changes();
        let peer = context.peer.clone();
        tokio::spawn(async move {
            while changes.changed().await.is_ok() {
                if peer.notify_tool_list_changed().await.is_err()
                    || peer.notify_prompt_list_changed().await.is_err()
                {
                    break;
                }
            }
        });
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        let mut accepted = SubscriptionFilter::default();
        accepted.tools_list_changed = requested.tools_list_changed;
        accepted.prompts_list_changed = requested.prompts_list_changed;
        Some(accepted)
    }

    /// The current protocol: tell the agent when its lists change, while it listens.
    async fn listen(&self, context: SubscriptionContext) -> Result<(), McpError> {
        let mut changes = self.daemon.changes();
        let tools = context.accepted().tools_list_changed == Some(true);
        let prompts = context.accepted().prompts_list_changed == Some(true);
        loop {
            tokio::select! {
                () = context.cancelled() => return Ok(()),
                changed = changes.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    if tools {
                        let _ = context.sink().notify_tool_list_changed().await;
                    }
                    if prompts {
                        let _ = context.sink().notify_prompt_list_changed().await;
                    }
                }
            }
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.note_client(&context);
        let mut tools = Session::own_tools();
        for server in self.catalogue().servers {
            let theirs = self.daemon.gateway().tools(&server.name);
            let names = tool_names(&server.name, theirs.iter().map(|t| t.name.as_ref()));
            for (mut tool, name) in theirs.into_iter().zip(names) {
                // Agents refuse a whole request over one bad tool name, or over two tools with
                // one name, so leave such tools out.
                match name {
                    Ok(name) => {
                        tool.name = name.into();
                        tools.push(tool);
                    }
                    Err(why) => self.daemon.left_out(&server.name, &tool.name, &why),
                }
            }
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.note_client(&context);
        let arg = |key: &str| {
            request
                .arguments
                .as_ref()
                .and_then(|a| a.get(key))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned()
        };
        let name = request.name.to_string();
        self.daemon.session_call(self.id, &name);
        let response = match name.as_str() {
            "find_skill" => self.find_skill(&arg("query")),
            "load_skill" => self.load_skill(&arg("name")),
            _ => {
                let Some((server, tool)) = name.split_once(SEPARATOR) else {
                    return Err(McpError::invalid_params(
                        format!("no tool named {name}"),
                        None,
                    ));
                };
                if !self.catalogue().servers.iter().any(|s| s.name == server) {
                    return Err(McpError::invalid_params(
                        format!("the server {server} is not on for you"),
                        None,
                    ));
                }
                match self
                    .daemon
                    .gateway()
                    .call(server, tool, request.arguments.clone())
                    .await
                {
                    Ok(result) => result.into(),
                    Err(e) => {
                        self.daemon
                            .session_error(self.id, &format!("{name}: {e:#}"));
                        error_result(format!("{e:#}"))
                    }
                }
            }
        };
        Ok(response)
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        self.note_client(&context);
        let catalogue = self.catalogue();
        let prompts: Vec<Prompt> = prompts(&catalogue)
            .into_iter()
            .map(|(name, item)| match item {
                Item::Skill(s) => Prompt::new(
                    name,
                    Some(format!("Use the {} skill. {}", s.name, s.description)),
                    Some(vec![
                        PromptArgument::new("task")
                            .with_description("What to use the skill for")
                            .with_required(false),
                    ]),
                ),
                Item::Command(c) => Prompt::new(
                    name,
                    Some(c.description.clone()),
                    Some(vec![
                        PromptArgument::new("arguments")
                            .with_description("Replaces $ARGUMENTS in the prompt")
                            .with_required(false),
                    ]),
                ),
            })
            .collect();
        Ok(ListPromptsResult::with_all_items(prompts))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, McpError> {
        let catalogue = self.catalogue();
        let arg = |key: &str| {
            request
                .arguments
                .as_ref()
                .and_then(|a| a.get(key))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned()
        };
        let named = prompts(&catalogue);
        let Some((_, item)) = named.iter().find(|(name, _)| *name == request.name) else {
            return Err(McpError::invalid_params(
                format!("no prompt named {}", request.name),
                None,
            ));
        };
        let text = match item {
            Item::Skill(skill) => {
                let task = arg("task");
                let mut text = format!(
                    "Use the skill below{}. Its files are in {}.\n\n{}",
                    if task.is_empty() {
                        String::new()
                    } else {
                        format!(" for this task: {task}")
                    },
                    skill.dir.display(),
                    skill.body()
                );
                text.truncate(text.trim_end().len());
                text
            }
            Item::Command(command) => command.render(&arg("arguments")),
        };
        self.daemon
            .session_call(self.id, &format!("prompt {}", request.name));
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)]).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_fit_what_agents_accept() {
        assert_eq!(
            tool_name("github", "create_issue").as_deref(),
            Some("github__create_issue")
        );
        assert_eq!(tool_name("server", "has.dot"), None);
        assert_eq!(tool_name("has.dot", "tool"), None);
        // No room left for a tool's name worth reading.
        assert_eq!(tool_name(&"s".repeat(40), &"x".repeat(50)), None);
        assert_eq!(tool_name("server", &"_".repeat(60)), None);
    }

    #[test]
    fn long_tool_names_are_shortened() {
        // A name that fits is left alone, even with the server's name in it.
        assert_eq!(
            tool_name("planetscale", "planetscale_get_branch").as_deref(),
            Some("planetscale__planetscale_get_branch")
        );
        assert_eq!(
            tool_name("planetscale", "planetscale_list_schema_recommendations").as_deref(),
            Some("planetscale__list_schema_recommendations")
        );
        // The exact name, as agents keep it in their settings from one version to the next.
        assert_eq!(
            tool_name(
                "planetscale",
                "planetscale_get_organization_billing_payment_method"
            )
            .as_deref(),
            Some("planetscale__get_organization_billing_payment_e051")
        );
        // Only a whole word is dropped from the front.
        let github = "github_create_pull_request_review_comment_reply_now";
        assert!(
            tool_name("git", github)
                .unwrap()
                .starts_with("git__github_create_")
        );
        // Two tools cut at the same place keep names of their own.
        let (a, b) = (
            format!("{}_a", "x".repeat(60)),
            format!("{}_b", "x".repeat(60)),
        );
        assert_ne!(tool_name("server", &a), tool_name("server", &b));
    }

    #[test]
    fn a_shortened_name_never_takes_one_that_fits() {
        let (long, fits) = (
            "planetscale_list_schema_recommendations",
            "list_schema_recommendations",
        );
        let seen = "planetscale__list_schema_recommendations";
        for tools in [[long, fits], [fits, long]] {
            let names = tool_names("planetscale", tools);
            let of = |tool| &names[tools.iter().position(|t| *t == tool).unwrap()];
            assert_eq!(of(fits).as_deref(), Ok(seen));
            assert_eq!(of(long), &Err(LeftOut::Taken));
            assert_eq!(own_tool_name("planetscale", tools, seen), Some(fits));
        }
        // Without the other, the long one gets the name, and calls to it reach the tool.
        assert_eq!(own_tool_name("planetscale", [long], seen), Some(long));
    }

    #[test]
    fn prompt_names_never_collide() {
        let skill = |name: &str| xolkit_core::Skill {
            name: name.into(),
            title: String::new(),
            description: String::new(),
            dir: Default::default(),
        };
        let catalogue = Catalogue {
            skills: vec![skill("p-review"), skill("p:review")],
            ..Catalogue::default()
        };
        let names: Vec<String> = prompts(&catalogue).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["p-review", "p-review-2"]);
    }

    #[test]
    fn prompt_names_lose_colons() {
        assert_eq!(prompt_name("tools:git:commit"), "tools-git-commit");
        assert_eq!(prompt_name("plain_name-2"), "plain_name-2");
    }
}
