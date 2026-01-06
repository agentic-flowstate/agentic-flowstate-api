use mcp_handlers::ToolHandler;
use once_cell::sync::OnceCell;
use std::sync::Arc;
use anyhow::Result;
use serde_json::Value;

// Global MCP handler instance
static MCP_HANDLER: OnceCell<Arc<ToolHandler>> = OnceCell::new();

// Initialize the handler (call this from main)
pub async fn init_mcp_handler() -> Result<()> {
    let handler = ToolHandler::new().await?;
    MCP_HANDLER.set(Arc::new(handler))
        .map_err(|_| anyhow::anyhow!("Failed to initialize MCP handler"))?;
    Ok(())
}

// Helper function to call MCP tools
pub async fn call_mcp_tool(tool_name: &str, arguments: Option<Value>) -> Result<Value> {
    let handler = MCP_HANDLER.get()
        .ok_or_else(|| anyhow::anyhow!("MCP handler not initialized"))?;
    handler.handle_tool_call(tool_name, arguments).await
}