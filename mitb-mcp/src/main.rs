use async_trait::async_trait;
use rust_mcp_sdk::macros::{JsonSchema, mcp_tool};
use rust_mcp_sdk::schema::{
    CallToolRequest, CallToolResult, Implementation, InitializeResult, LATEST_PROTOCOL_VERSION,
    ListToolsRequest, ListToolsResult, RpcError, ServerCapabilities, ServerCapabilitiesTools,
    TextContent, schema_utils::CallToolError,
};
use rust_mcp_sdk::{
    McpServer, StdioTransport, TransportOptions,
    error::SdkResult,
    mcp_server::{ServerHandler, ServerRuntime, server_runtime},
    tool_box,
};

#[tokio::main]
async fn main() -> SdkResult<()> {
    let server_details = InitializeResult {
        server_info: Implementation {
            name: "mitb-mcp".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            title: Some("MITB Reply MCP".to_string()),
        },
        capabilities: ServerCapabilities {
            tools: Some(ServerCapabilitiesTools { list_changed: None }),
            ..Default::default()
        },
        meta: None,
        instructions: Some(
            "Exposes a tool for the agent to submit a reply. Call `reply` with \
             the text you want to include in the reply"
                .to_string(),
        ),
        protocol_version: LATEST_PROTOCOL_VERSION.to_string(),
    };

    let transport = StdioTransport::new(TransportOptions::default())?;
    let handler = ReplyHandler {};
    let server: std::sync::Arc<ServerRuntime> =
        server_runtime::create_server(server_details, transport, handler);

    if let Err(start_error) = server.start().await {
        eprintln!(
            "{}",
            start_error
                .rpc_error_message()
                .unwrap_or(&start_error.to_string())
        );
    }
    Ok(())
}

#[mcp_tool(
    name = "reply",
    description = "Submit a reply. Pass the text you want included in the reply as the `text` argument.",
    title = "Reply",
    idempotent_hint = false,
    destructive_hint = true,
    open_world_hint = false,
    read_only_hint = false
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, JsonSchema)]
pub struct Reply {
    /// The text to include in the reply.
    text: String,
}

impl Reply {
    pub async fn call_tool(&self) -> Result<CallToolResult, CallToolError> {
        let path = std::env::current_dir()
            .map_err(|_| CallToolError("Failed to determine current working directory.".into()))?
            .join(".mitb-reply");
        tokio::fs::write(&path, self.text.as_str())
            .await
            .map_err(|_e| CallToolError("Failed to write reply.".into()))?;
        Ok(CallToolResult::text_content(vec![TextContent::from(
            "Reply submitted successfully.",
        )]))
    }
}

pub struct ReplyHandler;

#[async_trait]
impl ServerHandler for ReplyHandler {
    async fn handle_list_tools_request(
        &self,
        _request: ListToolsRequest,
        _runtime: std::sync::Arc<dyn McpServer>,
    ) -> std::result::Result<ListToolsResult, RpcError> {
        Ok(ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: ReplyTools::tools(),
        })
    }

    async fn handle_call_tool_request(
        &self,
        request: CallToolRequest,
        _runtime: std::sync::Arc<dyn McpServer>,
    ) -> std::result::Result<CallToolResult, CallToolError> {
        let tool_params: ReplyTools =
            ReplyTools::try_from(request.params).map_err(CallToolError::new)?;

        match tool_params {
            ReplyTools::Reply(exec) => exec.call_tool().await,
        }
    }
}

tool_box!(ReplyTools, [Reply]);
