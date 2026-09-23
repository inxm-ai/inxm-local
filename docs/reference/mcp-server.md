# Local MCP server

MCP lets compatible clients, including coding agents, use INXM Local's workflow capabilities through a shared interface. This matters because a client can compile, execute, inspect, repair, and schedule the same saved plans as the desktop app, while INXM Local continues to validate plans and control deterministic execution. It makes workflows reusable from an agent without handing the agent a separate execution path.

## Server endpoint

The application starts a loopback HTTP MCP server at `http://127.0.0.1:39387/mcp` by default. It also exposes `GET /health`, which returns a short status response. Change the port in Settings or `settings.json`.

The server accepts MCP JSON-RPC requests for `initialize`, `ping`, `tools/list`, and `tools/call`. It is unauthenticated and restricted to loopback callers and loopback Host or Origin headers.

## Available tools

| Tool | Required purpose |
| --- | --- |
| `compile_plan` | Compile and save a plan from an intent |
| `list_plans` | List stored plans |
| `show_plan` | Show a plan by ID prefix or name |
| `export_plan` | Export a plan bundle |
| `import_plan` | Import an inline plan bundle |
| `edit_plan` | Propose an edit to a plan |
| `execute_plan` | Execute a plan with inputs |
| `list_runs` | List recent runs |
| `inspect_run` | Inspect a run |
| `repair_run` | Propose a repair patch |
| `list_patches` | List repair patches |
| `schedule_plan` | Create a schedule with captured inputs |
| `list_schedules` | List schedules |
| `delete_schedule` | Delete a schedule |
| `set_schedule_enabled` | Enable or disable a schedule |

## Execute a plan

Call `execute_plan` with a plan reference and an input object:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "execute_plan",
    "arguments": {
      "plan_ref": "my-plan",
      "inputs": {"query": "Rust workflow engines", "limit": 5}
    }
  }
}
```

Missing, unknown, or incorrectly typed inputs are rejected before execution. Tool results include MCP `content` text and `structuredContent` JSON.

## Resume human interaction

When a plan needs an answer, the result has `status: "elicitation_required"`, a persisted `run_id`, and an `elicitation` object. Call `execute_plan` again with the same `run_id` and answers keyed by step ID:

```json
{
  "plan_ref": "my-plan",
  "run_id": "run-id-from-response",
  "human_responses": {"approve": true}
}
```

Completed steps are not repeated. Approval responses accept booleans, yes/no strings, or decision objects; free-text responses accept a string or an object with `text`.

## Run MCP-only mode

Use this when a native window is unavailable:

```sh
INXM_MCP_ONLY=1 inxm-local
```

For a repository self-test, run:

```sh
INXM_MCP_SELF_TEST=1 INXM_LOCAL_DATA_DIR=target/mcp-self-test cargo run
```
