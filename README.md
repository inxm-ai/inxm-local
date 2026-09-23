# INXM // local

A local-first Rust desktop app for compiled-AI workflows.

> The LLM is the **compiler**, not the runtime. You describe intent in chat, the compiler produces a typed plan, and a deterministic executor runs it. No AI improvisation in the execution path.

## What it does

- **Chat to create plans** — type plain language and the configured LLM turns it into a validated, versioned plan. Use an API key, an existing Codex/Claude Code login, or a compatible local/hosted endpoint. Slash commands (`/run`, `/plans`, `/repair`, …) drive everything else, with an animated command palette (type `/`, Tab to complete).
- **Plan-owned conversations** — every plan has one persistent chat. Opening a plan or one of its runs navigates to that chat instead of inserting a card into the currently open conversation. A fixed workspace card keeps plan controls, live progress, details, and complete execution history visible above the scrollable transcript.
- **Reusable, typed plan inputs** — compiled plans declare values supplied by each trigger (for example query, target, recipient, limit, or environment). Inputs are validated, available as `${input.<name>}`, persisted with runs, and captured independently by each schedule.
- **Deterministic runs** — the ported soloplayer executor runs steps in topological order, persists state and resolved inputs after every step, and streams live progress into the plan card.
- **Human-in-the-loop** — `HUMAN_INTERACTION` steps pause the run and ask in chat (Approve / Reject buttons or a free-text answer).
- **Repair loop** — a failed run can be handed back to the compiler (`/repair <run-id>`); the proposed patch appears as a card you apply or reject. Applied patches create a new plan version.
- **MCP management in the UI** — the *MCP Tools* view lists the tool catalog and lets you add / edit / delete local stdio or remote Streamable HTTP MCP servers (plus subprocess and HTTP tools). Changes persist to `tools.yaml` in the data dir.
- **Local HTTP MCP server** — the desktop client starts a local MCP server on launch so other clients can compile, find/show, execute, repair, edit, schedule, and inspect workflows through the same deterministic core.

## Quick start

Install the latest release on macOS or Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.sh | sh
```

On Windows, run the PowerShell installer:

```powershell
irm https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.ps1 | iex
```

The installer can also register INXM Local's local MCP server with your coding agents in the same step, so they can compile and run INXM workflows right away:

```bash
# register with every agent found on this machine
curl -fsSL https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.sh | sh -s -- --agents

# or pick specific agents
curl -fsSL https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.sh | sh -s -- --claude --codex --cursor
```

Then open INXM Local, choose a compiler connection under **Settings -> Compiler**, and describe a workflow in chat. The [first workflow tutorial](https://inxm-ai.github.io/inxm-local/getting-started/first-workflow/) walks through the complete path.

## Documentation

- [User documentation](https://inxm-ai.github.io/inxm-local/): install, configure, run, schedule, and troubleshoot workflows.
- [Developer documentation](https://inxm-ai.github.io/inxm-local/development/): set up the repository, understand the architecture, integrate agents, and run checks.

The [GitHub Releases page](https://github.com/inxm-ai/inxm-local/releases/latest) contains platform packages, and the [full installation guide](https://inxm-ai.github.io/inxm-local/getting-started/install/) covers manual and agent-registration installs.

## Telemetry (anonymous, opt-out at setup)

INXM Local sends two anonymous events, both only at app start — a launch ping (app version, OS name, launch mode) and a batched usage summary: plain tallies of plans created/edited and runs succeeded/failed/healed (split by app vs. MCP), the configured backend and model *name* (never a custom CLI's command or executable), the experimental-mode flag, and foreground seconds per view. No identifiers, no timestamps, no plan or user data, and no real-time tracking — counters accumulate in an inspectable local file (`telemetry-usage.json`) and are only sent on the next launch. The first-run setup screen discloses this with a pre-checked box: uncheck it there to opt out **before anything is ever sent** (nothing is collected while that screen is open). Installs that never see the setup screen — upgrades from older versions, headless/agent installs — send nothing.

Turn it off anytime via *Settings → Anonymous usage ping*, `"telemetry_enabled": false` in `settings.json`, `INXM_TELEMETRY=off`, or the `--no-telemetry` flag. Sends are fire-and-forget and can never affect normal operation.

Everything is inspectable: the exact event schema ([`src/telemetry/schema.rs`](src/telemetry/schema.rs)), the only sending code ([`src/telemetry/sender.rs`](src/telemetry/sender.rs)), and the complete Cloudflare Worker sink ([`telemetry-worker/`](telemetry-worker/), ~90-day retention in Workers Analytics Engine). Full details: [`docs/reference/telemetry.md`](docs/reference/telemetry.md).

## Contributing

See the [contribution guide](https://inxm-ai.github.io/inxm-local/development/contributing/) for development checks, pull request requirements, and the contributor license agreement.

## License

Copyright 2026 INXM GmbH. Licensed under the [Apache License, Version 2.0](LICENSE). Third-party notices for bundled fonts are listed in [assets/fonts/LICENSES.md](assets/fonts/LICENSES.md).
