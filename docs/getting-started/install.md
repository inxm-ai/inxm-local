# Install INXM Local

Install a release package when you want to use the desktop application.

---

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

---

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

---

## Verify the installation

Open INXM Local. The app should show the chat view. Select a compiler connection under **Settings -> Compiler** before compiling a plan. If the local MCP server is enabled, its status and port are shown in the application settings.

---

## Register coding agents

The standard installers above install INXM Local only. Connecting a coding agent such as Claude Code or Codex is an optional way to use INXM Local: the agent can create and manage reusable workflow plans for you through the local MCP server.

To register every supported agent already detected on your machine, add `--agents` to the installer:
```sh
curl -fsSL https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.sh | sh -s -- --agents
```

To register one specific agent, pass its flag instead, for example:

```sh
curl -fsSL https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.sh | sh -s -- --claude
```

Available agent flags are `--claude`, `--codex`, `--gemini`, `--qwen`,
`--copilot`, `--vscode`, `--cursor`, `--windsurf`, `--cline`, `--roo`,
`--opencode`, `--goose`, `--hermes`, `--pi`, and `--zed`.

See [Use INXM Local with coding agents](../user-guide/coding-agents.md) for the complete guide.

---

<h2>What's next</h2>

- [Create your first workflow](first-workflow.md)
- [Configure tools and MCP servers](../user-guide/tools.md)
