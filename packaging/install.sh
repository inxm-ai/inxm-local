#!/bin/sh
# Bootstrap installer for INXM Local. Downloads the latest GitHub release for
# this machine, installs it per-user, and optionally registers INXM Local's
# MCP server with coding agents: Claude Code, Codex, Gemini CLI, Qwen Code,
# Copilot CLI, VS Code (Copilot), Cursor, Windsurf, Cline, Roo Code,
# OpenCode, Goose, Hermes, Pi, and Zed.
#
#   curl -fsSL https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.sh | sh
#
# Options (pass after `sh -s --` when piping):
#   --agents       register with every agent found on this machine
#   --claude --codex --gemini --qwen --copilot --vscode --cursor --windsurf
#   --cline --roo --opencode --goose --hermes --pi --zed
#                  register with specific agents
#   --autostart    start INXM Local hidden at login (Linux only)
#   --uninstall    remove a previous install and agent registrations
#   --version X    install release vX instead of the latest
#
# Environment overrides:
#   INXM_MCP_URL   MCP endpoint to register (default http://127.0.0.1:39387/mcp)
#   PREFIX         Linux install root (default $HOME/.local)
set -eu

REPO="inxm-ai/inxm-local"
RAW_BASE="https://raw.githubusercontent.com/${REPO}/main"
MCP_URL="${INXM_MCP_URL:-http://127.0.0.1:39387/mcp}"
MCP_NAME="inxm-local"

AGENT_LIST="claude codex gemini qwen copilot vscode cursor windsurf cline roo opencode goose hermes pi zed"
for a in $AGENT_LIST; do eval "WANT_$a=0"; done
AGENTS_AUTO=0 AUTOSTART=0 UNINSTALL=0 VERSION=""
while [ $# -gt 0 ]; do
    case "$1" in
        --claude|--codex|--gemini|--qwen|--copilot|--vscode|--cursor|--windsurf|--cline|--roo|--opencode|--goose|--hermes|--pi|--zed)
            eval "WANT_${1#--}=1" ;;
        --agents) AGENTS_AUTO=1 ;;
        --autostart) AUTOSTART=1 ;;
        --uninstall) UNINSTALL=1 ;;
        --version) shift; VERSION="${1:?--version needs a value}" ;;
        -h|--help)
            cat <<'EOF'
Usage: install.sh [options]
  --agents       register the MCP server with every agent found on this machine
  --claude --codex --gemini --qwen --copilot --vscode --cursor --windsurf
  --cline --roo --opencode --goose --hermes --pi --zed
                 register with specific agents
  --autostart    start INXM Local hidden at login (Linux only)
  --uninstall    remove a previous install and agent registrations
  --version X    install release vX instead of the latest
Env: INXM_MCP_URL (MCP endpoint), PREFIX (Linux install root)
EOF
            exit 0 ;;
        *) echo "Unknown option: $1 (try --help)" >&2; exit 2 ;;
    esac
    shift
done

log() { printf '\033[1m[inxm]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[inxm]\033[0m %s\n' "$*" >&2; }
have() { command -v "$1" >/dev/null 2>&1; }

fetch() {
    # fetch URL DEST — download with curl or wget, whichever exists.
    if have curl; then
        curl -fL --proto '=https' --tlsv1.2 -o "$2" "$1"
    elif have wget; then
        wget -qO "$2" "$1"
    else
        warn "Neither curl nor wget is available."; exit 1
    fi
}

release_url() {
    # release_url ASSET — latest release, or a pinned tag with --version.
    if [ -n "$VERSION" ]; then
        echo "https://github.com/${REPO}/releases/download/v${VERSION#v}/$1"
    else
        echo "https://github.com/${REPO}/releases/latest/download/$1"
    fi
}

json_mcp() {
    # json_mcp add|remove FILE TOPKEY ENTRY_JSON — merge or delete the
    # $MCP_NAME entry under TOPKEY in a JSON config, preserving the rest.
    # Prints "added" / "exists" / "manual" on stdout for the caller.
    if ! have python3; then
        warn "python3 is required to edit $2 — add this ${MCP_NAME} entry manually under \"$3\": ${4:-}"
        echo manual
        return 0
    fi
    ACTION="$1" FILE="$2" TOPKEY="$3" ENTRY="${4:-}" NAME="$MCP_NAME" python3 - <<'PY' || true
import json, os
action, path, top = os.environ["ACTION"], os.environ["FILE"], os.environ["TOPKEY"]
name = os.environ["NAME"]
data = {}
if os.path.exists(path):
    with open(path) as f:
        text = f.read().strip()
    data = json.loads(text) if text else {}
if action == "add":
    servers = data.setdefault(top, {})
    if name in servers:
        print("exists")
        raise SystemExit
    servers[name] = json.loads(os.environ["ENTRY"])
else:
    if name not in data.get(top, {}):
        raise SystemExit
    del data[top][name]
os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
with open(path, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
if action == "add":
    print("added")
PY
}

# Agent config locations. Cline and Roo Code live inside VS Code's
# globalStorage; the VS Code user directory differs per OS.
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
    Darwin) VSCODE_USER="$HOME/Library/Application Support/Code/User" ;;
    *) VSCODE_USER="${XDG_CONFIG_HOME:-$HOME/.config}/Code/User" ;;
esac
CLINE_CONFIG="$VSCODE_USER/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json"
ROO_CONFIG="$VSCODE_USER/globalStorage/rooveterinaryinc.roo-cline/settings/mcp_settings.json"
VSCODE_CONFIG="$VSCODE_USER/mcp.json"
OPENCODE_CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/opencode/opencode.json"
GOOSE_CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/goose/config.yaml"
CODEX_CONFIG="$HOME/.codex/config.toml"
CURSOR_CONFIG="$HOME/.cursor/mcp.json"
WINDSURF_CONFIG="$HOME/.codeium/windsurf/mcp_config.json"
COPILOT_CONFIG="$HOME/.copilot/mcp-config.json"
GEMINI_CONFIG="$HOME/.gemini/settings.json"
QWEN_CONFIG="$HOME/.qwen/settings.json"
PI_SKILL_DIR="$HOME/.pi/agent/skills/use-inxm-mcp"
CLAUDE_SKILL_DIR="$HOME/.claude/skills/use-inxm-mcp"
ZED_CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/zed/settings.json"
ZED_SKILL_DIR="$HOME/.agents/skills/use-inxm-mcp"

detect_claude()   { have claude; }
detect_codex()    { have codex || [ -d "$HOME/.codex" ]; }
detect_gemini()   { have gemini || [ -d "$HOME/.gemini" ]; }
detect_qwen()     { have qwen || [ -d "$HOME/.qwen" ]; }
detect_copilot()  { have copilot || [ -d "$HOME/.copilot" ]; }
detect_vscode()   { [ -d "$VSCODE_USER" ]; }
detect_cursor()   { [ -d "$HOME/.cursor" ]; }
detect_windsurf() { [ -d "$HOME/.codeium/windsurf" ]; }
detect_cline()    { [ -d "$(dirname "$CLINE_CONFIG")" ]; }
detect_roo()      { [ -d "$(dirname "$ROO_CONFIG")" ]; }
detect_opencode() { have opencode || [ -d "$(dirname "$OPENCODE_CONFIG")" ]; }
detect_goose()    { have goose || [ -f "$GOOSE_CONFIG" ]; }
detect_hermes()   { have hermes; }
detect_pi()       { have pi || [ -d "$HOME/.pi" ]; }
detect_zed()      { have zed || [ -d "$(dirname "$ZED_CONFIG")" ]; }

install_skill() {
    # install_skill DIR — the use-inxm-mcp skill teaches skill-aware agents
    # (Claude Code, Pi, ...) how to drive the INXM MCP endpoint.
    mkdir -p "$1"
    fetch "$RAW_BASE/skills/use-inxm-mcp/SKILL.md" "$1/SKILL.md"
}

register_claude() {
    have claude || { warn "Claude Code CLI not found — install it, then rerun with --claude."; return; }
    if claude mcp get "$MCP_NAME" >/dev/null 2>&1; then
        log "Claude Code: MCP server '$MCP_NAME' already registered."
    else
        claude mcp add --scope user --transport http "$MCP_NAME" "$MCP_URL"
        log "Claude Code: registered '$MCP_NAME' -> $MCP_URL"
    fi
    install_skill "$CLAUDE_SKILL_DIR"
    log "Claude Code: installed the 'use-inxm-mcp' skill."
}

register_codex() {
    if grep -qs "mcp_servers.${MCP_NAME}" "$CODEX_CONFIG" 2>/dev/null; then
        log "Codex: MCP server '$MCP_NAME' already configured."
    else
        mkdir -p "$(dirname "$CODEX_CONFIG")"
        printf '\n[mcp_servers.%s]\nurl = "%s"\n' "$MCP_NAME" "$MCP_URL" >> "$CODEX_CONFIG"
        log "Codex: added '$MCP_NAME' -> $MCP_URL to ~/.codex/config.toml"
    fi
}

register_hermes() {
    have hermes || { warn "Hermes CLI not found — see docs/integration/hermes.md for manual setup."; return; }
    if hermes mcp add "$MCP_NAME" --url "$MCP_URL"; then
        log "Hermes: registered '$MCP_NAME' -> $MCP_URL"
    else
        warn "Hermes: 'hermes mcp add' failed — add it to ~/.hermes/config.yaml manually (see docs/integration/hermes.md)."
    fi
}

register_json() {
    # register_json LABEL FILE TOPKEY ENTRY_JSON
    case "$(json_mcp add "$2" "$3" "$4")" in
        added) log "$1: added '$MCP_NAME' -> $MCP_URL to $2" ;;
        exists) log "$1: MCP server '$MCP_NAME' already configured." ;;
        manual) ;;
        *) warn "$1: could not update $2 (invalid JSON?) — add '$MCP_NAME' there manually." ;;
    esac
}

register_opencode() {
    register_json "OpenCode" "$OPENCODE_CONFIG" mcp \
        "{\"type\": \"remote\", \"url\": \"$MCP_URL\", \"enabled\": true}"
}

register_cline() {
    [ -d "$(dirname "$CLINE_CONFIG")" ] || { warn "Cline settings not found (is the VS Code extension installed?)."; return; }
    register_json "Cline" "$CLINE_CONFIG" mcpServers \
        "{\"type\": \"streamableHttp\", \"url\": \"$MCP_URL\", \"disabled\": false, \"autoApprove\": []}"
}

register_pi() {
    # Pi has no native MCP client config; its skills directory is the
    # supported way to teach it the INXM endpoint.
    install_skill "$PI_SKILL_DIR"
    log "Pi: installed the 'use-inxm-mcp' skill into ~/.pi/agent/skills."
}

register_zed() {
    register_json "Zed" "$ZED_CONFIG" context_servers "{\"url\": \"$MCP_URL\"}"
    install_skill "$ZED_SKILL_DIR"
    log "Zed: installed the 'use-inxm-mcp' skill into ~/.agents/skills (global, every project)."
}

register_cursor() {
    register_json "Cursor" "$CURSOR_CONFIG" mcpServers "{\"url\": \"$MCP_URL\"}"
}

register_gemini() {
    register_json "Gemini CLI" "$GEMINI_CONFIG" mcpServers "{\"httpUrl\": \"$MCP_URL\"}"
}

register_qwen() {
    register_json "Qwen Code" "$QWEN_CONFIG" mcpServers "{\"httpUrl\": \"$MCP_URL\"}"
}

register_copilot() {
    register_json "Copilot CLI" "$COPILOT_CONFIG" mcpServers "{\"type\": \"http\", \"url\": \"$MCP_URL\"}"
}

register_vscode() {
    [ -d "$VSCODE_USER" ] || { warn "VS Code user directory not found — is VS Code installed?"; return; }
    # User-level mcp.json uses a top-level "servers" object.
    register_json "VS Code (Copilot)" "$VSCODE_CONFIG" servers "{\"type\": \"http\", \"url\": \"$MCP_URL\"}"
}

register_windsurf() {
    register_json "Windsurf" "$WINDSURF_CONFIG" mcpServers "{\"serverUrl\": \"$MCP_URL\"}"
}

register_roo() {
    [ -d "$(dirname "$ROO_CONFIG")" ] || { warn "Roo Code settings not found (is the VS Code extension installed?)."; return; }
    register_json "Roo Code" "$ROO_CONFIG" mcpServers \
        "{\"type\": \"streamable-http\", \"url\": \"$MCP_URL\", \"disabled\": false, \"alwaysAllow\": []}"
}

register_goose() {
    # Goose config is YAML; edit it only when PyYAML is available so the
    # rest of the file survives untouched.
    if have python3 && python3 -c 'import yaml' 2>/dev/null; then
        RESULT="$(GOOSE_FILE="$GOOSE_CONFIG" NAME="$MCP_NAME" URL="$MCP_URL" python3 - <<'PY'
import os, yaml
path, name, url = os.environ["GOOSE_FILE"], os.environ["NAME"], os.environ["URL"]
data = {}
if os.path.exists(path):
    with open(path) as f:
        data = yaml.safe_load(f) or {}
exts = data.setdefault("extensions", {})
if name in exts:
    print("exists")
else:
    exts[name] = {"enabled": True, "name": name, "type": "streamable_http", "uri": url}
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "w") as f:
        yaml.safe_dump(data, f, sort_keys=False)
    print("added")
PY
)"
        if [ "$RESULT" = "added" ]; then
            log "Goose: added '$MCP_NAME' -> $MCP_URL to $GOOSE_CONFIG"
        else
            log "Goose: MCP server '$MCP_NAME' already configured."
        fi
    else
        warn "Goose: PyYAML not available — add this to $GOOSE_CONFIG under 'extensions:' manually:"
        warn "  ${MCP_NAME}: {enabled: true, name: ${MCP_NAME}, type: streamable_http, uri: ${MCP_URL}}"
    fi
}

# ---------------------------------------------------------------- uninstall
if [ "$UNINSTALL" -eq 1 ]; then
    case "$OS" in
        Linux)
            PREFIX="${PREFIX:-$HOME/.local}"
            rm -f "$PREFIX/bin/inxm-local" \
                "$PREFIX/share/icons/hicolor/512x512/apps/ai.inxm.local.png" \
                "$PREFIX/share/applications/ai.inxm.local.desktop" \
                "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/ai.inxm.local.desktop"
            rm -rf "$PREFIX/share/doc/inxm-local"
            log "Removed INXM Local from $PREFIX (user data is kept)."
            ;;
        Darwin)
            rm -rf "$HOME/Applications/INXM Local.app"
            rm -f "$HOME/.local/bin/inxm-local"
            log "Removed INXM Local from ~/Applications (user data is kept)."
            ;;
        *) warn "Unsupported platform for uninstall: $OS"; exit 1 ;;
    esac
    if have claude; then
        claude mcp remove --scope user "$MCP_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$CLAUDE_SKILL_DIR" "$PI_SKILL_DIR" "$ZED_SKILL_DIR"
    have hermes && hermes mcp remove "$MCP_NAME" >/dev/null 2>&1 || true
    # File and its top-level MCP key, separated by '|' (paths contain spaces).
    for cfg_top in "$OPENCODE_CONFIG|mcp" "$CLINE_CONFIG|mcpServers" \
        "$ROO_CONFIG|mcpServers" "$VSCODE_CONFIG|servers" \
        "$CURSOR_CONFIG|mcpServers" "$WINDSURF_CONFIG|mcpServers" \
        "$COPILOT_CONFIG|mcpServers" "$GEMINI_CONFIG|mcpServers" \
        "$QWEN_CONFIG|mcpServers" "$ZED_CONFIG|context_servers"; do
        cfg="${cfg_top%|*}"; top="${cfg_top##*|}"
        [ -f "$cfg" ] && json_mcp remove "$cfg" "$top" || true
    done
    if [ -f "$GOOSE_CONFIG" ] && have python3 && python3 -c 'import yaml' 2>/dev/null; then
        GOOSE_FILE="$GOOSE_CONFIG" NAME="$MCP_NAME" python3 - <<'PY' || true
import os, yaml
path, name = os.environ["GOOSE_FILE"], os.environ["NAME"]
with open(path) as f:
    data = yaml.safe_load(f) or {}
if name in data.get("extensions", {}):
    del data["extensions"][name]
    with open(path, "w") as f:
        yaml.safe_dump(data, f, sort_keys=False)
PY
    fi
    grep -qs "mcp_servers.${MCP_NAME}" "$CODEX_CONFIG" 2>/dev/null && \
        warn "Codex: remove the [mcp_servers.${MCP_NAME}] block from ~/.codex/config.toml manually."
    log "Agent MCP registrations removed."
    exit 0
fi

# ------------------------------------------------------------------ install
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

case "$OS" in
    Linux)
        case "$ARCH" in
            x86_64|amd64) TARGET="x86_64-unknown-linux-gnu" ;;
            *) warn "No prebuilt Linux package for $ARCH — build from source: https://github.com/${REPO}#build-from-source"; exit 1 ;;
        esac
        ASSET="inxm-local-${TARGET}.tar.gz"
        log "Downloading $ASSET ..."
        fetch "$(release_url "$ASSET")" "$TMP/$ASSET"
        tar -xzf "$TMP/$ASSET" -C "$TMP"
        if [ "$AUTOSTART" -eq 1 ]; then
            sh "$TMP/inxm-local/install.sh" --autostart
        else
            sh "$TMP/inxm-local/install.sh"
        fi
        BIN="${PREFIX:-$HOME/.local}/bin/inxm-local"
        ;;
    Darwin)
        case "$ARCH" in
            arm64) TARGET="aarch64-apple-darwin" ;;
            x86_64) TARGET="x86_64-apple-darwin" ;;
            *) warn "No prebuilt macOS package for $ARCH"; exit 1 ;;
        esac
        ASSET="inxm-local-${TARGET}.app.zip"
        log "Downloading $ASSET ..."
        fetch "$(release_url "$ASSET")" "$TMP/$ASSET"
        mkdir -p "$HOME/Applications"
        rm -rf "$HOME/Applications/INXM Local.app"
        ditto -x -k "$TMP/$ASSET" "$HOME/Applications"
        # CLI/headless symlink so `inxm-local --headless` works from a shell.
        mkdir -p "$HOME/.local/bin"
        ln -sf "$HOME/Applications/INXM Local.app/Contents/MacOS/inxm-local" \
            "$HOME/.local/bin/inxm-local"
        BIN="$HOME/.local/bin/inxm-local"
        log "Installed INXM Local to ~/Applications."
        ;;
    *)
        warn "Unsupported platform: $OS."
        warn "On Windows, run: irm ${RAW_BASE}/packaging/install.ps1 | iex"
        exit 1
        ;;
esac

case ":${PATH}:" in
    *":$(dirname "$BIN"):"*) ;;
    *) warn "$(dirname "$BIN") is not on your PATH — add it to run 'inxm-local' from a shell." ;;
esac

# --------------------------------------------------------- agent integration
FOUND_ANY=0
for a in $AGENT_LIST; do
    if [ "$AGENTS_AUTO" -eq 1 ] && "detect_$a"; then eval "WANT_$a=1"; fi
    eval "[ \"\$WANT_$a\" -eq 1 ]" && FOUND_ANY=1
done
if [ "$AGENTS_AUTO" -eq 1 ] && [ "$FOUND_ANY" -eq 0 ]; then
    log "No known agents found on this machine — skipping MCP registration."
fi

for a in $AGENT_LIST; do
    eval "[ \"\$WANT_$a\" -eq 1 ]" && "register_$a" || true
done

log "Done. Start the app (or 'inxm-local --headless') so agents can reach $MCP_URL"
