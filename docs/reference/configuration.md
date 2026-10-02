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
| `INXM_SKIP_SHELL_ENV` | Do not import the login shell environment at startup |
| `INXM_TELEMETRY=off` | Disable telemetry for the process; `0`, `false`, and `no` also disable it |
| `--no-telemetry` | Disable telemetry for the process |
| `--set-telemetry=on` or `--set-telemetry=off` | Record a first-run telemetry choice and exit; does not overwrite a completed onboarding or installer choice |
| `INXM_TELEMETRY_ENDPOINT` | Override the telemetry event endpoint |
| `ANTHROPIC_API_KEY` | API-key fallback for Claude when the backend is automatic or API-backed |
| `OPENAI_API_KEY` | API-key fallback for OpenAI when the backend is automatic or API-backed |

## Tool environment

Subprocess tools, Local stdio MCP servers, AI-generated code steps, and agent CLIs run as programs on your machine. Each program always receives:

| Source | What is passed |
| --- | --- |
| Tool inputs (subprocess tools only) | `INXM_ARGS` with the full JSON input, and one `INXM_ARG_<NAME>` per input |
| The tool's own variables (**Env** or **Server env**) | Exactly what you set. These values take precedence over inherited ones. |

It also inherits INXM Local's environment, which depends on how INXM Local was started:

| How INXM Local was started | What is inherited |
| --- | --- |
| From the desktop, a login item, or a service manager such as launchd or systemd | `PATH`, plus the certificate, proxy, locale, and runtime variables from your shell profile listed in [Variables copied from your shell profile](#variables-copied-from-your-shell-profile). **No credentials.** |
| From a terminal | All of the terminal's variables, **including credentials** |

HTTP tools and Remote HTTP MCP servers do not start a local program, so none of this applies to them.

### Variables copied from your shell profile

Apps opened from Finder, the Dock, the Start menu, a login item, a desktop launcher, or a desktop MCP client do not run your shell startup files, such as `~/.zshrc`, `~/.bash_profile`, `config.fish`, or your PowerShell profile. To make up for this, INXM Local starts your login shell once at startup and copies the variables tools need to run. On Windows it uses PowerShell 7 (`pwsh`) when installed, otherwise Windows PowerShell.

Only these variables are copied:

| Purpose | Variables |
| --- | --- |
| Program locations | `PATH` |
| Certificates | `NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`, `SSL_CERT_DIR`, `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE` |
| Proxies | `HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`, `ALL_PROXY`, and their lowercase forms |
| Locale | `LANG`, `LC_ALL`, `LC_CTYPE` |
| Runtimes | `JAVA_HOME`, `GOPATH`, `GOROOT`, `CARGO_HOME`, `RUSTUP_HOME`, `DOTNET_ROOT`, `NVM_DIR`, `NVM_BIN`, `VOLTA_HOME`, `PNPM_HOME`, `BUN_INSTALL`, `PYENV_ROOT`, `ASDF_DIR`, `ASDF_DATA_DIR`, `MISE_DATA_DIR` |

**Tokens, API keys, and other credentials in your profile are not copied.** When INXM Local is opened from the desktop, AI-generated steps and launched tools cannot read them. A proxy variable whose URL embeds a username or password (for example `http://user:password@proxy:8080`) is not copied either; set it on the tools that need it.

**Starting INXM Local from a terminal passes on all of that terminal's variables, including credentials.** Every program started from a terminal inherits its full environment. This is how the operating system works, not something INXM Local adds. If your shell exports `OPENAI_API_KEY`, `GITHUB_TOKEN`, or `AWS_SECRET_ACCESS_KEY`, a terminal-launched INXM Local has them. So does every process it starts, including INXM Local stdio MCP servers, subprocess tools, AI-generated steps, and agent CLIs.

Choose the launch method that matches what your tools should see:

- To keep credentials away from launched tools and AI-generated steps, open INXM Local from the desktop. Set each credential only in the environment variables of the tool that needs it, such as **Server env** for Local stdio MCP servers. Tools that worked from a terminal only because they inherited a variable need that variable moved into their own environment.
- To give launched tools your full shell environment, start INXM Local from a terminal, for example with `inxm-local`.

How the import works:

- Variables INXM Local already has keep their values. The shell only adds the listed ones that are missing.
- On macOS and Linux, `PATH` follows your shell's order, then any other entries the app already had. On Windows, the registry `PATH` comes first, then any entries your PowerShell profile adds.
- A tool's own environment variables override both.
- When you start INXM Local from a terminal on macOS or Linux, it already has that terminal's environment and skips the import.
- If your shell does not finish within 10 seconds, INXM Local continues with its own environment and logs a warning.
- `INXM_SKIP_SHELL_ENV=1` turns the import off.

Changes to your shell profile apply after you fully quit and reopen INXM Local. Closing the window to the tray does not restart the app.
