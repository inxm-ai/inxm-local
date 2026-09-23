# Configuration reference

INXM Local stores settings, plans, runs, schedules, and the tool catalog in a platform data directory.

## Default data locations

The application uses the platform data directory for `ai/inxm/inxm-local`. If the platform directory cannot be resolved, it falls back to `.inxm-local/` in the current working directory. The directory contains `settings.json`, the tool catalog (`tools.yaml`), schedules, plans, runs, and telemetry usage counters when telemetry is enabled.

## Compiler settings

The Settings view persists these `settings.json` fields:

| Field | Purpose |
| --- | --- |
| `backend` | `auto`, `claude`, `open_ai`, `codex`, `claude_code`, `google_vertex`, `open_ai_compatible`, `anthropic_compatible`, or `custom_cli` |
| `model` | Model identifier override; empty uses the backend default |
| `api_key` | API key entered in the app; empty uses the backend environment variable |
| `api_base` | Base URL for compatible or Vertex connections |
| `executable` | Executable override for CLI-backed connections |
| `command_template` | Custom CLI command; `{{PROMPT}}` marks the prompt position |
| `custom_cli_agentic` | Allows the experimental agent step for an explicitly agentic custom CLI |
| `experimental_agent_calls` | Enables `AGENT_CALL` steps for supported account CLIs or agentic custom CLIs |
| `auto_mode` | Skips the guided design approval gate |
| `max_tokens` | Maximum compiler output tokens; `0` uses the backend default |
| `mcp_port` | Loopback MCP port; default `39387` |
| `keep_running_in_background` | Controls close-to-tray behavior |
| `schedules_paused` | Pauses all schedules without changing their individual enabled state |
| `telemetry_enabled` | Explicit telemetry consent; only `true` enables collection |

Other fields control theme, update checks, onboarding state, and Codex sandbox behavior. Prefer the Settings UI so defaults and compatibility behavior remain intact.

## Environment variables and entry points

| Variable or flag | Behavior |
| --- | --- |
| `INXM_LOCAL_DATA_DIR` | Override the data directory |
| `INXM_HEADLESS=1` or `--headless` | Run the MCP server and scheduler without a window |
| `INXM_MCP_ONLY=1` | Run only the MCP endpoint without a window |
| `INXM_MCP_SELF_TEST=1` | Run the MCP self-test path |
| `INXM_AGENT_MODE=1` or `--agent-mode` | Use the reduced interface intended for installations driven by agents through MCP |
| `INXM_START_HIDDEN` or `--start-hidden` | Start the desktop app hidden in the system tray |
| `INXM_FORCE_WAYLAND` | On Linux, disable the X11 preference used for tray lifecycle behavior |
| `INXM_TELEMETRY=off` | Disable telemetry for the process; `0`, `false`, and `no` also disable it |
| `--no-telemetry` | Disable telemetry for the process |
| `--set-telemetry=<value>` | Set telemetry consent from the command line and exit |
| `INXM_TELEMETRY_ENDPOINT` | Override the telemetry event endpoint |
| `ANTHROPIC_API_KEY` | API-key fallback for Claude when the backend is automatic or API-backed |
| `OPENAI_API_KEY` | API-key fallback for OpenAI when the backend is automatic or API-backed |
