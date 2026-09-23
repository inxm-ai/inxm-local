# Install INXM Local

Install a release package when you want to use the desktop application.

## Quick install

The recommended way to install INXM Local is to use the quick install command for your operating system. It downloads the latest release and installs it per-user (no root needed).

### 🍎 macOS

```sh
curl -fsSL https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.sh | sh
```

### 🪟 Windows

```powershell
irm https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.ps1 | iex
```

### 🐧 Linux

```sh
curl -fsSL https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.sh | sh
```

The scripts install per user. The Unix installer supports `--version`, `--autostart`, `--agents`, individual agent flags, and `--uninstall`. Existing agent configuration is merged and registration is idempotent. Use `INXM_MCP_URL` when registering an endpoint other than the default.

## Manual installation

Download a package from [GitHub Releases](https://github.com/inxm-ai/inxm-local/releases/latest):

### 🍎 macOS

Choose `aarch64` for Apple Silicon or `x86_64` for Intel, unzip the `.app.zip`, and open the app.

### 🪟 Windows

Run the `x86_64-pc-windows-msvc-setup.exe` installer.

### 🐧 Linux

- Debian or Ubuntu: install the `x86_64-unknown-linux-gnu.deb` package with `sudo apt install ./<package>.deb`.
- Without root: use the release `.tar.gz` and its `install.sh` script.

Release builds are not notarized or signed. macOS may require **Open Anyway** in **System Settings -> Privacy & Security**, or removal of the quarantine attribute after unzipping:

```sh
xattr -dr com.apple.quarantine ~/Applications/"INXM Local.app"
```

Windows SmartScreen may require **More info -> Run anyway** or:

```powershell
Unblock-File .\inxm-local-x86_64-pc-windows-msvc-setup.exe
```

## Verify the installation

Open INXM Local. The app should show the chat view. Select a compiler connection under **Settings -> Compiler** before compiling a plan. If the local MCP server is enabled, its status and port are shown in the application settings.


## Register coding agents

The standard installers above install INXM Local only. Connecting a coding agent such as Claude Code or Codex is an optional way to use INXM Local: the agent can create and manage reusable workflow plans for you through the local MCP server.

### 🍎 macOS

The macOS installer is a shell script. To register every supported agent already detected on your machine, add `--agents`:

```sh
curl -fsSL https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.sh | sh -s -- --agents
```

To register one specific agent, pass its flag instead, for example `--claude`:

```sh
curl -fsSL https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.sh | sh -s -- --claude
```

Refer to the [Supported agents](#supported-agents) section for more details on the available flags and switches.

### 🪟 Windows

Download [`install.ps1`](../../packaging/install.ps1), then run it in PowerShell. To register every supported agent already detected on your machine, use `-Agents`:

```powershell
.\install.ps1 -Agents
```

To register one specific agent, use its switch instead, for example `-Claude`:

```powershell
.\install.ps1 -Claude
```

Refer to the [Supported agents](#supported-agents) section for more details on the available flags and switches.

### 🐧 Linux

The Linux installer uses the same shell commands as macOS. To register every supported agent already detected on your machine, add `--agents`:

```sh
curl -fsSL https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.sh | sh -s -- --agents
```

To register one specific agent, pass its flag instead, for example `--claude`:

```sh
curl -fsSL https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.sh | sh -s -- --claude
```

Refer to the [Supported agents](#supported-agents) section for more details on the available flags and switches.

### Supported agents

The Unix flags and Windows switches below perform the same registration. Existing configuration files are merged rather than overwritten, and registration is idempotent: rerunning the installer does not duplicate entries.

| macOS/Linux flag | Windows switch | Agent | Registration |
| --- | --- | --- | --- |
| `--claude` | `-Claude` | Claude Code | Runs `claude mcp add` at user scope and installs the [`use-inxm-mcp` skill](../../skills/use-inxm-mcp/SKILL.md). |
| `--codex` | `-Codex` | Codex CLI | Adds the MCP server to the Codex configuration. |
| `--gemini` | `-Gemini` | Gemini CLI | Adds the MCP server to the Gemini settings. |
| `--qwen` | `-Qwen` | Qwen Code | Adds the MCP server to the Qwen settings. |
| `--copilot` | `-Copilot` | GitHub Copilot CLI | Adds the MCP server to the Copilot configuration. |
| `--vscode` | `-VSCode` | VS Code (Copilot) | Adds the MCP server to the user-level `mcp.json`. |
| `--cursor` | `-Cursor` | Cursor | Adds the MCP server to the Cursor configuration. |
| `--windsurf` | `-Windsurf` | Windsurf | Adds the MCP server to the Windsurf configuration. |
| `--cline` | `-Cline` | Cline | Adds the MCP server to the Cline settings. |
| `--roo` | `-Roo` | Roo Code | Adds the MCP server to the Roo Code settings. |
| `--opencode` | `-OpenCode` | OpenCode | Adds the MCP server to the OpenCode configuration. |
| `--goose` | `-Goose` | Goose | Adds the MCP server to the Goose configuration. |
| `--hermes` | `-Hermes` | Hermes | Runs `hermes mcp add`; see the [Hermes integration guide](../integration/hermes.md). |
| `--pi` | `-Pi` | Pi | Installs the `use-inxm-mcp` skill; Pi has no native MCP configuration. |
| `--zed` | `-Zed` | Zed | Adds the MCP server to Zed and installs the `use-inxm-mcp` skill. |

See [Use INXM Local with coding agents](../user-guide/coding-agents.md) for the complete guide.

Other useful flag options:

| Option | What it does |
| --- | --- | 
| `--autostart` | Linux: start INXM Local hidden at login. |
| `--version 0.1.0` | Pin the installation to a specific release. |
| `--uninstall` | Remove the app and agent registrations. |

<h2>What's next</h2>

- [Create your first workflow](first-workflow.md)
- [Configure tools and MCP servers](../user-guide/tools.md)
