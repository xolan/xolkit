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
        let lines: Vec<String> = found
            .iter()
            .take(15)
            .map(|item| match item {
                Item::Skill(s) => format!("- {} (skill): {}", s.name, s.description),
                Item::Command(c) => format!(
                    "- {} (prompt, run it as /mcp__xolkit__{}): {}",
                    c.name,
                    prompt_name(&c.name),
                    c.description
                ),
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

    fn accepted_subscription_filter(&self, requested: &SubscriptionFilter) -> Option<SubscriptionFilter> {
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
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools = Session::own_tools();
        for server in self.catalogue().servers {
            for tool in self.daemon.gateway().tools(&server.name) {
                let mut tool = tool.clone();
                tool.name = format!("{}{SEPARATOR}{}", server.name, tool.name).into();
                tools.push(tool);
            }
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
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
                    return Err(McpError::invalid_params(format!("no tool named {name}"), None));
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
                        self.daemon.session_error(self.id, &format!("{name}: {e:#}"));
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
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        let catalogue = self.catalogue();
        let mut prompts: Vec<Prompt> = catalogue
            .skills
            .iter()
            .map(|s| {
                Prompt::new(
                    prompt_name(&s.name),
                    Some(format!("Use the {} skill. {}", s.name, s.description)),
                    Some(vec![
                        PromptArgument::new("task")
                            .with_description("What to use the skill for")
                            .with_required(false),
                    ]),
                )
            })
            .collect();
        prompts.extend(catalogue.commands.iter().map(|c| {
            Prompt::new(
                prompt_name(&c.name),
                Some(c.description.clone()),
                Some(vec![
                    PromptArgument::new("arguments")
                        .with_description("Replaces $ARGUMENTS in the prompt")
                        .with_required(false),
                ]),
            )
        }));
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
        let text = if let Some(skill) = catalogue
            .skills
            .iter()
            .find(|s| prompt_name(&s.name) == request.name)
        {
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
        } else if let Some(command) = catalogue
            .commands
            .iter()
            .find(|c| prompt_name(&c.name) == request.name)
        {
            command.render(&arg("arguments"))
        } else {
            return Err(McpError::invalid_params(
                format!("no prompt named {}", request.name),
                None,
            ));
        };
        self.daemon.session_call(self.id, &format!("prompt {}", request.name));
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)]).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_names_lose_colons() {
        assert_eq!(prompt_name("tools:git:commit"), "tools-git-commit");
        assert_eq!(prompt_name("plain_name-2"), "plain_name-2");
    }
}
