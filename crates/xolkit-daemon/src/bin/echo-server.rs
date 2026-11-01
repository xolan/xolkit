//! A tiny MCP server for the end-to-end tests: one tool, `echo`, over stdio.

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};

struct Echo;

impl ServerHandler for Echo {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let schema = serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}});
        Ok(ListToolsResult::with_all_items(vec![Tool::new(
            "echo",
            "Says the text back",
            std::sync::Arc::new(schema.as_object().cloned().unwrap_or_default()),
        )]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let text = request
            .arguments
            .and_then(|a| a.get("text").and_then(|t| t.as_str()).map(str::to_owned))
            .unwrap_or_default();
        Ok(CallToolResult::success(vec![ContentBlock::text(format!("echo: {text}"))]).into())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let running = Echo.serve(rmcp::transport::io::stdio()).await?;
    running.waiting().await?;
    Ok(())
}
