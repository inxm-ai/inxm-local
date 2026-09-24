# Work with coding agents

The INXM Orchestrator is designed to work with coding agents as well as through its own interface. The agent remains the conversational partner; INXM provides a shared workflow engine that turns an intent into a validated, reusable plan and runs that plan deterministically. This separates an agent's reasoning from execution that needs to be repeatable and inspectable.

INXM Local exposes this workflow capability through a local Model Context Protocol (MCP) server. A compatible coding agent connects to that server and can use the same plans and runs as the desktop app.

## How the interaction works

1. You ask the coding agent to create or use a workflow with INXM Local.
2. The agent calls INXM Local tools through MCP. MCP exposes tools for compiling, listing, showing, editing, importing, exporting, and scheduling plans, as well as executing, inspecting, and repairing runs.
3. INXM validates and saves plans. When a plan runs, its deterministic executor follows the saved steps and persists progress.
4. The agent can report results, inspect failures, or help you review a proposed repair. A workflow can also pause for an explicitly declared human response.

The agent does not receive a separate execution path that bypasses INXM's plan validation or run state. MCP is the connection protocol; the [MCP server reference](../reference/mcp-server.md) lists the exact tools and request behavior.

## Connect a coding agent

The [Register coding agents section](../getting-started/install.md#register-coding-agents) lists supported installer options. For compatible clients, the installer can also add the reusable `use-inxm-mcp` [skill](https://github.com/inxm-ai/inxm-local/blob/main/skills/use-inxm-mcp/SKILL.md), which provides agent-facing guidance for using INXM Local plans and runs.

INXM Local's default MCP endpoint is:

```text
http://127.0.0.1:39387/mcp
```

The port can be changed in Settings. The endpoint is unauthenticated and restricted to loopback; connect only trusted local clients. See [MCP server security and behavior](../reference/mcp-server.md#server-endpoint).
