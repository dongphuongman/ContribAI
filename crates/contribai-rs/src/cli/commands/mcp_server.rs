//! Handles `Commands::McpServer` — start MCP server for Claude/Antigravity integration.

use crate::cli::{create_github, create_memory, load_config};

pub async fn run_mcp_server(config_path: Option<&str>, allow_writes: bool) -> anyhow::Result<()> {
    // MCP uses stdout for JSON-RPC — all human output goes to stderr
    eprintln!("🔌 ContribAI MCP server starting on stdio...");
    eprintln!("   Waiting for client connection...\n");
    if allow_writes {
        eprintln!("   WRITE CAPABILITY ENABLED; upstream PRs still require repository opt-in.\n");
    } else {
        eprintln!("   Read-only capability set (default).\n");
    }

    let config = load_config(config_path)?;
    let github = create_github(&config)?;
    let memory = create_memory(&config)?;

    contribai::mcp::server::run_stdio_server_with_capabilities(&github, &memory, allow_writes)
        .await?;
    Ok(())
}
