# INXM // local

A local-first, Rust desktop app for compiled-AI workflows

> The LLM is the **compiler**, not the runtime. You describe intent in chat,
> the compiler produces a typed plan, and a deterministic executor runs it.
> No AI improvisation in the execution path.

## What it does

- **Chat to create plans** — type plain language and the configured LLM
  turns it into a validated, versioned plan. Use an API key, an existing
  Codex/Claude Code login, or a compatible local/hosted endpoint. Slash
  commands (`/run`, `/plans`, `/repair`, …) drive everything else, with an
  animated command palette (type `/`, Tab to complete).
- **Plan-owned conversations** — every plan has one persistent chat. Opening
  a plan or one of its runs navigates to that chat instead of inserting a card
  into the currently open conversation. A fixed workspace card keeps plan
  controls, live progress, details, and complete execution history visible
  above the scrollable transcript.
- **Reusable, typed plan inputs** — compiled plans declare values supplied by
  each trigger (for example query, target, recipient, limit, or environment).
  Inputs are validated, available as `${input.<name>}`, persisted with runs,
  and captured independently by each schedule.
- **Deterministic runs** — the ported soloplayer executor runs steps in
  topological order, persists state and resolved inputs after every step, and
  streams live progress into the plan card.
- **Human-in-the-loop** — `HUMAN_INTERACTION` steps pause the run and ask in
  chat (Approve / Reject buttons or a free-text answer).
- **Repair loop** — a failed run can be handed back to the compiler
  (`/repair <run-id>`); the proposed patch appears as a card you apply or
  reject. Applied patches create a new plan version.
- **MCP management in the UI** — the *MCP Tools* view lists the tool
  catalog and lets you add / edit / delete local stdio or remote Streamable
  HTTP MCP servers (plus subprocess and HTTP tools). Changes persist to
  `tools.yaml` in the data dir.
- **Local HTTP MCP server** — the desktop client starts a local MCP server on
  launch so other clients can compile, find/show, execute, repair, edit,
  schedule, and inspect workflows through the same deterministic core.

## Install

### Quick install (recommended)

On Linux and macOS, this downloads the latest release for your machine and
installs it per-user (no root needed):

```bash
curl -fsSL https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.sh | sh
```

On Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.ps1 | iex
```

The installer can also register INXM Local's local MCP server with your
coding agents in the same step, so they can compile and run INXM workflows
right away:

```bash
# register with every agent found on this machine
curl -fsSL https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.sh | sh -s -- --agents

# or pick specific agents
curl -fsSL https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.sh | sh -s -- --claude --codex --cursor
```

Supported agents and what the installer does for each:

| Flag | Agent | Registration |
| --- | --- | --- |
| `--claude` | Claude Code | `claude mcp add` (user scope) + installs the [`use-inxm-mcp` skill](skills/use-inxm-mcp/SKILL.md) into `~/.claude/skills` |
| `--codex` | Codex CLI | `[mcp_servers.inxm-local]` entry in `~/.codex/config.toml` |
| `--gemini` | Gemini CLI | `mcpServers` entry in `~/.gemini/settings.json` |
| `--qwen` | Qwen Code | `mcpServers` entry in `~/.qwen/settings.json` |
| `--copilot` | GitHub Copilot CLI | `mcpServers` entry in `~/.copilot/mcp-config.json` |
| `--vscode` | VS Code (Copilot) | `servers` entry in the user-level `mcp.json` |
| `--cursor` | Cursor | `mcpServers` entry in `~/.cursor/mcp.json` |
| `--windsurf` | Windsurf | `mcpServers` entry in `~/.codeium/windsurf/mcp_config.json` |
| `--cline` | Cline | `mcpServers` entry in `cline_mcp_settings.json` |
| `--roo` | Roo Code | `mcpServers` entry in `mcp_settings.json` |
| `--opencode` | OpenCode | `mcp` entry in `~/.config/opencode/opencode.json` |
| `--goose` | Goose | `extensions` entry in `~/.config/goose/config.yaml` |
| `--hermes` | Hermes | `hermes mcp add` (see the [Hermes integration guide](docs/integration/hermes.md)) |
| `--pi` | Pi | installs the `use-inxm-mcp` skill into `~/.pi/agent/skills` (Pi has no native MCP config) |
| `--zed` | Zed | `context_servers` entry in `~/.config/zed/settings.json` + installs the [`use-inxm-mcp` skill](skills/use-inxm-mcp/SKILL.md) into `~/.agents/skills` |

Existing config files are merged, not overwritten, and registration is
idempotent — rerunning the installer never duplicates entries. Set
`INXM_MCP_URL` to register a non-default endpoint.

On Windows, download [`install.ps1`](packaging/install.ps1) and run it with
`-Agents` or the matching switches (`-Claude`, `-Cursor`, ...) for the same
registrations.

Other useful flags: `--autostart` (Linux: start hidden at login),
`--version 0.1.0` (pin a release), `--uninstall` (remove the app and agent
registrations). Both scripts are also attached to each release, so
`https://github.com/inxm-ai/inxm-local/releases/latest/download/install.sh`
works too.

### Manual install

Download the latest package for your platform from the
**[GitHub Releases page](https://github.com/inxm-ai/inxm-local/releases/latest)**.

- **Windows (x86-64):** download and run the `.exe` installer. It installs
  per-user (no admin rights needed) and can optionally register INXM Local
  to start hidden in the system tray when you log in. SmartScreen will warn
  about the unsigned installer — see
  [unsigned builds](#unsigned-builds-macos-gatekeeper--windows-smartscreen).
- **macOS:** download the `.app.zip` for Apple Silicon (`aarch64`) or Intel
  (`x86_64`), unzip it, and open **INXM Local**. No admin rights needed —
  drop it in `~/Applications` if you can't write to `/Applications`.
  Gatekeeper will block the un-notarized app — see
  [unsigned builds](#unsigned-builds-macos-gatekeeper--windows-smartscreen).
- **Linux (Debian/Ubuntu, x86-64):** download the `.deb` and install it with
  `sudo apt install ./inxm-local-x86_64-unknown-linux-gnu.deb`. For a
  per-user install without root, download the `.tar.gz` instead, unpack it,
  and run `./install.sh` (add `--autostart` to start INXM Local at login;
  `--uninstall` removes it again).

### Unsigned builds: macOS Gatekeeper & Windows SmartScreen

Our releases are not yet signed with an Apple Developer ID or a Windows code
signing certificate, so both operating systems will warn about (or block)
builds downloaded with a browser. The builds are safe — every release is built
from this repository by [GitHub Actions](.github/workflows/release.yml). Until
we have signing set up, use these workarounds:

**macOS** — a browser-downloaded app is quarantined, and because the app is
not notarized, macOS reports it as "damaged" or says it "cannot be opened
because Apple could not verify" it. Either:

- Remove the quarantine flag after unzipping:

  ```bash
  xattr -dr com.apple.quarantine ~/Applications/"INXM Local.app"
  ```

- Or try to open the app once, then go to **System Settings → Privacy &
  Security**, scroll down to the blocked-app notice, and click **Open Anyway**.
- Or use the [quick install](#quick-install-recommended) script — `curl`
  downloads don't set the quarantine flag, so the app opens normally.

**Windows** — SmartScreen shows "Windows protected your PC" when you run the
unsigned installer. Either:

- Click **More info → Run anyway** in the SmartScreen dialog.
- Or clear the mark-of-the-web before running it:

  ```powershell
  Unblock-File .\inxm-local-x86_64-pc-windows-msvc-setup.exe
  ```

Some corporate policies block unsigned executables entirely
(SmartScreen set to "Warn and prevent bypass", or AppLocker rules). In that
case ask your IT admin to allowlist the installer, or build from source with
`cargo build --release`.

### Linux one-liner

On Debian or Ubuntu, this one-liner downloads and installs the latest x86-64
release:

```bash
curl -fL https://github.com/inxm-ai/inxm-local/releases/latest/download/inxm-local-x86_64-unknown-linux-gnu.deb -o /tmp/inxm-local.deb && sudo apt install -y /tmp/inxm-local.deb
```

Then launch `inxm-local` from your application menu or terminal.

Open **Settings → Compiler** and choose one connection:

- **Claude API / OpenAI API** — enter an API key or set
  `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` before starting the app.
- **OpenAI account / Claude account** — install and sign in to the `codex` or
  `claude` CLI. The app invokes the CLI non-interactively, so no API key is
  stored in INXM.
- **Custom OpenAI URL / Custom Anthropic URL** — enter a base URL and model.
  API keys are optional, allowing local servers such as Ollama, LM Studio,
  llama.cpp, vLLM, or another compatible gateway. For an OpenAI-compatible
  server, enter its API root (for example `http://localhost:11434/v1`), not
  the full `/chat/completions` path.

The selected connection and model are shared by plan compilation, repair/edit
requests, and `PROMPT_CALL` steps during execution. Existing `settings.json`
files remain compatible and continue to use their configured Claude/OpenAI
API-key backend.

Data (plans, runs, patches, `tools.yaml`) lives in the platform data dir
(`~/.local/share/inxm-local/` on Linux); override with
`INXM_LOCAL_DATA_DIR=/path`. A starter catalog with an `echo` tool is
seeded on first launch. An example catalog is in `examples-config/tools.yaml`.

### Chat commands

| Command | Effect |
|---|---|
| *(plain text)* / `/compile <intent>` | Compile a plan in a new chat; refine the owned plan in a linked chat |
| `/plans`, `/runs`, `/tools` | List plans / runs / catalog |
| `/show <plan>` | Open the plan in its owned chat (id prefix or name) |
| `/run <plan> [--inputs '<json>']` | Execute a plan with invocation inputs |
| `/inspect <run-id>` | Step status, timing, errors of a run |
| `/repair <run-id>` | Propose a patch for a failed run |
| `/apply <patch-id>` / `/reject <patch-id> [reason]` | Resolve a patch |
| `/schedule <plan> <cron> [--inputs '<json>']` / `/schedules` | Create / list schedules with captured inputs |
| `/help`, `/clear` | Help / clear chat |

## Running schedules in the background

The desktop app keeps running in the system tray when **Keep schedules running
in the background** is enabled under Settings. The option turns on
automatically when an enabled schedule exists. Use the tray menu to reopen the
window, pause or resume all schedules without changing their individual state,
or quit the process completely.

For servers and unattended machines, headless mode runs the MCP server **and**
the scheduler without a window:

```bash
inxm-local --headless          # or: INXM_HEADLESS=1 inxm-local
```

Keep it running after logout with your platform's service manager, e.g.:

```bash
nohup inxm-local --headless >/tmp/inxm-headless.log 2>&1 &
```

or a systemd user unit (`~/.config/systemd/user/inxm-local.service`):

```ini
[Unit]
Description=INXM // local headless scheduler

[Service]
ExecStart=%h/.local/bin/inxm-local --headless
Restart=on-failure

[Install]
WantedBy=default.target
```

(`systemctl --user enable --now inxm-local`). On Windows, use Task Scheduler
with the same `--headless` argument.

Only one scheduler runs per data dir: a `scheduler.lock` file (holding the
owner's PID) guards against the desktop app and a headless instance firing
the same schedule twice. The second instance detects a live holder and skips
its scheduler; stale locks from crashed processes are reclaimed
automatically. Missed slots while nothing was running are not caught up, by
design.

## Local HTTP MCP server

The app starts a local Streamable-HTTP-style MCP server when the desktop
client starts. By default it listens only on loopback:

```text
http://127.0.0.1:39387/mcp
```

The port is stored in `settings.json` and can be changed under
**Settings → Local MCP server**. If startup cannot bind the port (for example,
another process is already using it), the sidebar/footer and Settings view show
a warning with the bind error. Choose another port, save settings, and restart
the app.

For automation or environments where a native window is unavailable, start
only the MCP endpoint. Startup succeeds with an explicit listening message or
exits non-zero with the bind error:

```sh
INXM_MCP_ONLY=1 inxm-local
```

A simple health endpoint is also available:

```text
GET http://127.0.0.1:39387/health
```

### MCP tools

Call tools via JSON-RPC `tools/call` at `/mcp`. The server also supports
`initialize`, `tools/list`, and `ping`.

| Tool | Purpose |
|---|---|
| `compile_plan` | Compile natural language into a validated, saved plan |
| `list_plans` | Find/list stored plans |
| `show_plan` | Show a plan by id, id prefix, or exact name |
| `export_plan` | Export a published plan as an importable bundle with tool references |
| `edit_plan` | Edit an existing plan using the configured compiler |
| `execute_plan` | Execute a plan with invocation inputs |
| `list_runs` | List recent runs |
| `inspect_run` | Inspect run step status, timing, output, and errors |
| `repair_run` | Propose a repair patch for a failed run |
| `list_patches` | List repair patches |
| `schedule_plan` | Schedule a plan using cron syntax |
| `list_schedules` | List configured schedules |

For coding agents, the repo includes a reusable
[`use-inxm-mcp` skill](skills/use-inxm-mcp/SKILL.md). See also the
[Hermes integration guide](docs/integration/hermes.md) for a complete
bidirectional agent example.

Example request:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "execute_plan",
    "arguments": {
      "plan_ref": "my-plan",
      "inputs": {
        "query": "Rust workflow engines",
        "limit": 5
      }
    }
  }
}
```

Tool responses include both MCP `content` text and `structuredContent` JSON.
`show_plan` and `list_plans` expose each plan's typed `inputs` contract.
`execute_plan.inputs` supplies values for one run; `schedule_plan.inputs`
validates and stores values that will be reused whenever that schedule fires.
Missing required, unknown, and incorrectly typed inputs are rejected before a
run or schedule is created. Defaults declared by the plan are applied and the
resolved values are included in `inspect_run` and `list_schedules` responses.

Plans reference invocation values as `${input.<name>}`. `${conf.<key>}` remains
available for static workflow implementation configuration. During compilation,
the planner is instructed to promote changeable details from the original
intent—such as search terms, subjects, URLs, recipients, date ranges, limits,
output destinations, environments, formats, thresholds, and behavior flags—to
input properties rather than hard-coding them or asking for them mid-run.

### Outbound remote MCP servers

The **MCP Tools** editor supports both local stdio servers and remote
Streamable HTTP endpoints. A remote entry without authentication has this
shape in `tools.yaml`:

```yaml
config:
  kind: mcp
  endpoint: https://mcp.example.com/mcp
  tool_name: search
```

Enable OAuth in the editor for protected endpoints. If the provider issued a
public client ID, enter it there; otherwise INXM uses dynamic client
registration when the provider supports it. The persisted configuration
contains policy only:

```yaml
config:
  kind: mcp
  endpoint: https://mcp.example.com/mcp
  auth:
    mode: oauth
    client_id: optional-public-client-id
  tool_name: search
```

Choose **Connect** to start authorization. INXM binds a one-use loopback
callback and shows an authorization link to open or copy. Authorization uses
the authorization-code flow with S256 PKCE, server discovery, and resource
indicators. Access and refresh tokens and dynamic client registrations are
stored only in the operating system credential vault; they are never written
to `tools.yaml`, settings, plans, or bundles. **Disconnect** removes the vault
entry.

Scheduled and headless executions only reuse or refresh credentials already in
the vault and never start an interactive flow. If a credential expires and
cannot be refreshed, or the server requires additional scopes, execution asks
you to reconnect under **MCP Tools**. OAuth endpoints must use HTTPS; loopback
HTTP is accepted for local development. If the OS credential vault is
unavailable, OAuth fails closed without a plaintext fallback.

### Human interaction / elicitations

Plans with `HUMAN_INTERACTION` steps expose the pause as an elicitation-shaped
structured response from `execute_plan` instead of reading from stdin. If a
human answer is needed and none was supplied, the tool returns:

```json
{
  "status": "elicitation_required",
  "message": "Provide an answer in execute_plan.human_responses keyed by step_id and call execute_plan again with this run_id.",
  "run_id": "persisted-run-id",
  "elicitation": {
    "step_id": "approve",
    "prompt": "Approve deployment?",
    "approval_required": true,
    "response_field": "approval",
    "schema": { "type": "boolean", "title": "Approve?" }
  }
}
```

The executor runs and persists every dependency before pausing, so prompts may
include resolved outputs such as a generated summary. The response also includes
a `run_id`. Call `execute_plan` again with that `run_id` and a
`human_responses` value keyed by `step_id`:

```json
{
  "plan_ref": "my-plan",
  "run_id": "run-id-from-elicitation-response",
  "human_responses": {
    "approve": true
  }
}
```

The same run resumes from its persisted checkpoint; completed fetches, tool
calls, and model calls are not repeated. If the plan reaches another human step,
the tool returns another `elicitation_required` response with the same run ID.

Approval steps accept booleans, yes/no strings, or decision objects such as
`{"decision":"approve"}` / `{"decision":"reject"}`. Free-text steps accept a
string or an object with a `text` field.

### Self-test the MCP server

A headless self-test starts the local MCP server with `cargo run`, connects to
it over HTTP, and exercises a logical flow: initialize, list tools, list/show a
seeded input plan, execute a live `echo` tool call with an invocation input,
inspect the persisted input, schedule the plan with a different captured input,
and list schedules.

```sh
INXM_MCP_SELF_TEST=1 INXM_LOCAL_DATA_DIR=target/mcp-self-test cargo run
```

Expected output includes:

```text
MCP self-test passed on http://127.0.0.1:<ephemeral-port>/mcp
```

## Architecture

```
src/
  llm.rs       — shared HTTP and account-CLI LLM transports
  compiler/   validator/   executor/   repair/   plan/   storage/   tools/
              — ported from inxm-soloplayer, unchanged in behaviour —
  app/
    engine.rs      — tokio thread; EngineCommand → EngineEvent bridge to egui
    mcp_server.rs  — local HTTP MCP server over the same workflow core
    theme.rs       — all design tokens (colors, spacing, type scale)
    anim.rs     — entrance/pulse helpers (time-based, id-keyed)
    widgets.rs  — atoms: badges, status dots, buttons, typing indicator
    views/      — organisms: chat, plan_card, plans index, mcp manager
    mod.rs      — shell: sidebar navigation, event routing
```

The UI thread never blocks: commands go to a dedicated tokio runtime;
events come back over a channel with `request_repaint`. Two hooks were
added to the ported executor (both `Option`al, stdin behaviour unchanged
when absent):

- `ExecutorConfig::progress` — per-step status stream for live plan cards
- `ExecutorConfig::human` — routes `HUMAN_INTERACTION` steps to chat
  instead of stdin

## Development

To build and run from source, install the Rust toolchain and the platform build
dependencies, then run:

```sh
cargo run --release
```

```sh
cargo test
cargo clippy --all-targets

# Optional: start and call the local HTTP MCP server end-to-end
INXM_MCP_SELF_TEST=1 INXM_LOCAL_DATA_DIR=target/mcp-self-test cargo run
```

## Telemetry (anonymous, opt-out at setup)

INXM Local sends two anonymous events, both only at app start — a launch
ping (app version, OS name, launch mode) and a batched usage summary: plain
tallies of plans created/edited and runs succeeded/failed/healed (split by
app vs. MCP), the configured backend and model *name* (never a custom CLI's
command or executable), the experimental-mode flag, and foreground seconds
per view. No identifiers, no timestamps, no plan or user data, and no
real-time tracking — counters accumulate in an inspectable local file
(`telemetry-usage.json`) and are only sent on the next launch. The
first-run setup screen discloses this with a pre-checked box: uncheck it
there to opt out **before anything is ever sent** (nothing is collected
while that screen is open). Installs that never see the setup screen —
upgrades from older versions, headless/agent installs — send nothing.

Turn it off anytime via *Settings → Anonymous usage ping*,
`"telemetry_enabled": false` in `settings.json`, `INXM_TELEMETRY=off`, or
the `--no-telemetry` flag. Sends are fire-and-forget and can never affect
normal operation.

Everything is inspectable: the exact event schema
([`src/telemetry/schema.rs`](src/telemetry/schema.rs)), the only sending
code ([`src/telemetry/sender.rs`](src/telemetry/sender.rs)), and the
complete Cloudflare Worker sink ([`telemetry-worker/`](telemetry-worker/),
~90-day retention in Workers Analytics Engine). Full details:
[`docs/telemetry.md`](docs/telemetry.md).

## Contributing

Contributions are welcome. Please read [CONTRIBUTING.md](CONTRIBUTING.md),
including the Contributor License Agreement, before opening a pull request.

## License

Copyright 2026 INXM GmbH. Licensed under the
[Apache License, Version 2.0](LICENSE). Third-party notices for bundled fonts
are listed in [assets/fonts/LICENSES.md](assets/fonts/LICENSES.md).
