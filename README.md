<div align="center">
	<img src="assets/favlogo.png" alt="INXM logo" width="192">
	<h1>INXM // local</h1>
	<h3>Local-first Rust desktop app for compiled-AI workflows</h3>
	<p>
		<strong>The LLM is the compiler, not the runtime.</strong><br>
		Describe intent in chat. INXM compiles a typed plan; a deterministic executor runs it.<br>
		No AI improvisation in the execution path.
	</p>
	<p>
		<a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License: Apache 2.0"></a>
		<a href="CONTRIBUTING.md"><img src="https://img.shields.io/badge/contributions-welcome-brightgreen" alt="contributions welcome"></a>
		<a href="https://www.inxm.ai/oss/inxm-local"><img src="https://img.shields.io/badge/website-inxm.ai-FF5900" alt="INXM Local website"></a>
		<img src="https://img.shields.io/badge/Discord-coming%20soon-5865F2?logo=discord&amp;logoColor=white" alt="Discord coming soon">
	</p>
</div>

## 🌟 Features

- **Build plans with AI** — Describe what you want in plain language and get a validated, versioned plan. Works with API keys, Codex/Claude Code logins, and compatible local or hosted LLMs.
- **Chat with each plan** — Every plan has its own persistent conversation, with controls, live progress, details, and execution history in one place.
- **Define reusable inputs** — Plans accept typed, validated inputs such as queries, targets, recipients, limits, and environments via `${input.<name>}`.
- **Run workflows deterministically** — Steps execute in topological order, with state and resolved inputs persisted after every step.
- **Pause for human input** — `HUMAN_INTERACTION` steps can request approval, rejection, or a free-text response before continuing.
- **Repair failed runs** — `/repair <run-id>` uses the compiler to propose a patch. Accepting it creates a new plan version.
- **Manage MCP tools** — Add, edit, and remove local stdio, remote Streamable HTTP MCP, subprocess, and HTTP tools directly from the UI. Configuration is stored in `tools.yaml`.
- **Expose workflows over MCP** — A built-in local HTTP MCP server lets other clients compile, find/show, run, repair, edit, schedule, and inspect workflows.

## 📝 Documentation

- [User documentation](https://inxm-ai.github.io/inxm-local/): install, configure, run, schedule, and troubleshoot workflows.
- [Developer documentation](https://inxm-ai.github.io/inxm-local/development/): set up the repository, understand the architecture, integrate agents, and run checks.

The [GitHub Releases page](https://github.com/inxm-ai/inxm-local/releases/latest) contains platform packages, and the [full installation guide](https://inxm-ai.github.io/inxm-local/getting-started/install/) covers manual and agent-registration installs.


## 🚀 Quick start

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

## 🔌 Telemetry (anonymous, opt-out at setup)

INXM Local sends two anonymous events, both only at app start — a launch ping (app version, OS name, launch mode) and a batched usage summary: plain tallies of plans created/edited and runs succeeded/failed/healed (split by app vs. MCP), the configured backend and model *name* (never a custom CLI's command or executable), the experimental-mode flag, and foreground seconds per view. No identifiers, no timestamps, no plan or user data, and no real-time tracking — counters accumulate in an inspectable local file (`telemetry-usage.json`) and are only sent on the next launch. The first-run setup screen discloses this with a pre-checked box: uncheck it there to opt out **before anything is ever sent** (nothing is collected while that screen is open). Installs that never see the setup screen — upgrades from older versions, headless/agent installs — send nothing.

Turn it off anytime via *Settings → Anonymous usage ping*, `"telemetry_enabled": false` in `settings.json`, `INXM_TELEMETRY=off`, or the `--no-telemetry` flag. Sends are fire-and-forget and can never affect normal operation.

Everything is inspectable: the exact event schema ([`src/telemetry/schema.rs`](src/telemetry/schema.rs)), the only sending code ([`src/telemetry/sender.rs`](src/telemetry/sender.rs)), and the complete Cloudflare Worker sink ([`telemetry-worker/`](telemetry-worker/), ~90-day retention in Workers Analytics Engine). Full details: [`docs/reference/telemetry.md`](docs/reference/telemetry.md).

## 🤝 Contributing

See the [contribution guide](https://inxm-ai.github.io/inxm-local/development/contributing/) for development checks, pull request requirements, and the contributor license agreement.

## License

Copyright 2026 INXM GmbH. Licensed under the [Apache License, Version 2.0](LICENSE). Third-party notices for bundled fonts are listed in [assets/fonts/LICENSES.md](assets/fonts/LICENSES.md).
