# Execution

Code, tools, and agents share files through a run workspace. Read this page to understand how execution workspaces and agent steps work.

## Execution workspaces

Local workflow processes share a run workspace so code, agents, and tools can hand off files without depending on the directory from which INXM Local was launched. The executor sets each child process's working directory; it never changes the application's process-wide directory.

### Why a shared workspace

An API connection does not execute every step through the same transport. For example, a Claude API workflow can fetch data with a tool, transform it with `CODE_CALL`, and launch Claude Code CLI for an `AGENT_CALL`. Without a consistent working directory, a file written by one process may not be visible at the same relative path to the next.

The shared workspace makes those local file handoffs consistent. It does not give a remote API filesystem access: a `PROMPT_CALL` still receives only its resolved prompt content. API-backed agents use a local CLI, as described in [Agent steps](#agent-steps).

### Workspace resolution

The executor resolves the workspace from `root_directory`:

| Input | Workspace |
| --- | --- |
| A non-empty directory path | The selected existing directory |
| An omitted optional input, `null`, or an empty string | `<data-dir>/runs/<run-id>/workspace` |

Workflows that depend on an existing repository or user files should require `root_directory`; workflows that create new artifacts or process supplied data can declare it optional.

The managed directory is created on demand when a local step needs it. An explicit root must already be a directory; an unavailable path fails instead of silently falling back to a new managed directory. Step subdirectories must also already exist.

With `INXM_LOCAL_DATA_DIR=/path/to/data`, a managed workspace is `/path/to/data/runs/<run-id>/workspace`, not the data-directory root. Separate managed runs have separate directories. Runs explicitly targeting the same project directory do not gain that isolation.

The resolved root is shared through the step execution context and exposed as `${input.root_directory}` in the runtime plan. Persisted invocation inputs remain unchanged: an omitted optional root is not rewritten to the generated absolute path in the run record.

### Which steps use it

| Execution path | Workspace behavior |
| --- | --- |
| `CODE_CALL` | Uses the run workspace by default; an explicit `working_dir` selects a directory |
| `AGENT_CALL`, including API-backed CLI execution | Resolves its required `working_dir` against the run workspace, normally through `${input.root_directory}` |
| Subprocess `TOOL_CALL` | Uses the run workspace unless the tool configuration specifies an override |
| Stdio MCP `TOOL_CALL` | Starts the local MCP server process in the run workspace |
| `FAN_OUT` children | Inherit the same run workspace; they do not receive separate per-item directories |
| HTTP tools and remote MCP servers | Do not acquire local workflow filesystem semantics |
| Standalone tool calls and MCP discovery | Keep their existing launch-directory behavior |

For local working-directory overrides, relative paths resolve against the run workspace and absolute paths remain absolute. The selected root must still be available for local steps, including those with directory overrides.

Stdio MCP configuration arguments are not rewritten. A filesystem server configured with `.` interprets it from the run workspace; an absolute allowlist remains unchanged and can prevent access to that workspace. The same applies to other relative server arguments, so tool configurations must not assume the application's launch directory.

### Resume and repair safety

Resuming a run reuses its workspace and intermediate files. Repair cannot change `root_directory` when completed local work is being reused, because later steps may need files from the original directory. Start a new run to use a different workspace in that case.

### Boundaries

A workspace is a default location, not an additional sandbox or access grant. Agent permissions, subprocess permissions, tool allowlists, and MCP filesystem restrictions still apply. Steps and fan-out iterations within one run share files; use explicit subdirectories or filenames when they must avoid collisions.

## Agent steps

When the compiler connection in **Settings → Compiler** uses Claude API or OpenAI API, agent steps still require the corresponding local CLI: Claude Code or Codex. The CLI provides the agent execution loop and local tools, using the connection's API key and model without a separate CLI login.

Agent steps are an experimental feature and only work when **Settings → Compiler → Experimental execution → Enable experimental agent steps** is enabled. If the CLI is not already installed, click **Install Claude Code CLI** or **Install Codex CLI**, then **Confirm installation**. INXM handles installation after confirmation; it does not install the CLI automatically when a plan runs.

### Connection routing

| Selected connection | Compilation and `PROMPT_CALL` | `AGENT_CALL` |
| --- | --- | --- |
| Claude API | Direct Anthropic API requests | Claude Code CLI using the selected API key and model |
| OpenAI API | Direct OpenAI Chat Completions requests | Codex CLI using the selected API key and model through the Responses API |
| Auto | The API selected from available environment keys (Claude first) | The matching Claude Code or Codex CLI with that API key |
| Claude account / OpenAI account | Selected account CLI | The same CLI with its existing authentication |

Compatible-URL and Vertex connections do not gain agent support through this routing.

### CLI installation

INXM discovers an existing CLI on `PATH` and in common installation folders, or uses an explicit CLI executable path. The built-in installer requires Node.js/npm and performs a global npm installation, downloading and executing third-party package code. It verifies the executable with `--version` and saves its path for the selected API connection.

### Credentials and execution boundaries

For API-backed agents, INXM takes the key from the selected connection's Settings value or its usual environment variable when that value is empty. The key is supplied to the child process, not in command-line arguments. INXM does not run a CLI login command or overwrite saved CLI credentials; usage is billed to the supplied key. Account-backed connections retain their existing authentication.

Agent steps can edit files and execute commands. Existing approval, timeout, audit, sandbox, and [workspace rules](#execution-workspaces) still apply.

CLI-reported usage contributes to [basic token counts](token-usage.md#basic-count). Its internal requests do not pass through INXM's direct API transport and are not individual entries in the detailed Claude API ledger.

<h2>What's next</h2>

- Read the [architecture overview](../development/architecture.md).
- See [Token usage](token-usage.md) for counting rules across API and CLI execution.
