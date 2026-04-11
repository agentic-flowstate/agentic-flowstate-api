/// Commands that agents must never execute.
///
/// These patterns use Claude Code's deny-rule glob syntax and are enforced at
/// the framework level via `disallowed_tools` on `ClaudeCodeOptions`. They
/// block *before* the LLM sees the tool result, even in `BypassPermissions`
/// mode. The agent receives feedback explaining the denial so it can use the
/// proper MCP tools instead.
///
/// Context: in-app agents previously killed the API server by running
/// `launchctl bootout` on `com.agentic.api`, which is the very process
/// hosting the conversation. This is a self-destruct loop — the bootout
/// sends SIGTERM, the conversation dies mid-tool-call, and the bootstrap
/// (restart) half of the command never executes, leaving the service
/// permanently deregistered.
pub fn disallowed_tools() -> Vec<String> {
    vec![
        // ── Service management ──────────────────────────────────────────
        // Prevent agents from stopping/unloading/removing agentic services.
        // The setup script uses `kickstart -k` which is safe and atomic;
        // bootout/unload/remove are destructive and deregister the service.
        "Bash(launchctl bootout *)".into(),
        "Bash(launchctl unload *)".into(),
        "Bash(launchctl remove *)".into(),
        // Prevent agents from registering/restarting services directly.
        // Use `mcp__agentic-mcp__manage_service` or `mcp__agentic-mcp__run_setup` instead.
        "Bash(launchctl bootstrap *)".into(),
        "Bash(launchctl kickstart *)".into(),
        "Bash(launchctl load *)".into(),
        // Prevent agents from killing processes by name or PID.
        // An agent running inside the API server must not kill its own host.
        "Bash(kill *)".into(),
        "Bash(pkill *)".into(),
        "Bash(killall *)".into(),
        // ── Repository creation ─────────────────────────────────────────
        // Repos MUST be created via `mcp__agentic-mcp__create_repo`, which
        // handles GitHub creation, local cloning, infrastructure scaffolding,
        // and repo registration in one atomic operation. Manual CLI repo
        // creation bypasses registration and infrastructure setup.
        //
        // git — local repo creation
        "Bash(git init)".into(),
        "Bash(git init *)".into(),
        // GitHub CLI (gh) — remote repo creation & forking
        "Bash(gh repo create*)".into(),
        "Bash(gh repo fork*)".into(),
        // GitLab CLI (glab) — remote repo creation & forking
        "Bash(glab repo create*)".into(),
        "Bash(glab project create*)".into(),
        "Bash(glab repo fork*)".into(),
        // Hub (deprecated GitHub CLI) — remote repo creation & forking
        "Bash(hub create*)".into(),
        "Bash(hub fork*)".into(),
        "Bash(hub init*)".into(),
    ]
}
