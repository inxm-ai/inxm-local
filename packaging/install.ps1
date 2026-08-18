# Bootstrap installer for INXM Local on Windows. Downloads the latest release
# setup, installs it silently per-user, and optionally registers INXM Local's
# MCP server with coding agents: Claude Code, Codex, Gemini CLI, Qwen Code,
# Copilot CLI, VS Code (Copilot), Cursor, Windsurf, Cline, Roo Code,
# OpenCode, Goose, Hermes, Pi, and Zed.
#
#   irm https://raw.githubusercontent.com/inxm-ai/inxm-local/main/packaging/install.ps1 | iex
#
# With options, download first, then run:
#   .\install.ps1 -Agents                   register every agent found
#   .\install.ps1 -Claude -Cursor -Hermes   register specific agents
#   .\install.ps1 -Version 0.1.0            install a pinned release
param(
    [switch]$Claude,
    [switch]$Codex,
    [switch]$Gemini,
    [switch]$Qwen,
    [switch]$Copilot,
    [switch]$VSCode,
    [switch]$Cursor,
    [switch]$Windsurf,
    [switch]$Cline,
    [switch]$Roo,
    [switch]$OpenCode,
    [switch]$Goose,
    [switch]$Hermes,
    [switch]$Pi,
    [switch]$Zed,
    [switch]$Agents,
    [string]$Version = ""
)

$ErrorActionPreference = "Stop"
$Repo = "inxm-ai/inxm-local"
$RawBase = "https://raw.githubusercontent.com/$Repo/main"
$McpName = "inxm-local"
$McpUrl = if ($env:INXM_MCP_URL) { $env:INXM_MCP_URL } else { "http://127.0.0.1:39387/mcp" }

$VSCodeUser = Join-Path $env:APPDATA "Code\User"
$Paths = @{
    ClineConfig    = Join-Path $VSCodeUser "globalStorage\saoudrizwan.claude-dev\settings\cline_mcp_settings.json"
    RooConfig      = Join-Path $VSCodeUser "globalStorage\rooveterinaryinc.roo-cline\settings\mcp_settings.json"
    VSCodeConfig   = Join-Path $VSCodeUser "mcp.json"
    OpenCodeConfig = Join-Path $env:USERPROFILE ".config\opencode\opencode.json"
    CodexConfig    = Join-Path $env:USERPROFILE ".codex\config.toml"
    CursorConfig   = Join-Path $env:USERPROFILE ".cursor\mcp.json"
    WindsurfConfig = Join-Path $env:USERPROFILE ".codeium\windsurf\mcp_config.json"
    CopilotConfig  = Join-Path $env:USERPROFILE ".copilot\mcp-config.json"
    GeminiConfig   = Join-Path $env:USERPROFILE ".gemini\settings.json"
    QwenConfig     = Join-Path $env:USERPROFILE ".qwen\settings.json"
    GooseConfig    = Join-Path $env:APPDATA "Block\goose\config\config.yaml"
    PiSkillDir     = Join-Path $env:USERPROFILE ".pi\agent\skills\use-inxm-mcp"
    ClaudeSkillDir = Join-Path $env:USERPROFILE ".claude\skills\use-inxm-mcp"
    ZedConfig      = Join-Path $env:APPDATA "Zed\settings.json"
    ZedSkillDir    = Join-Path $env:USERPROFILE ".agents\skills\use-inxm-mcp"
}

function Log($msg) { Write-Host "[inxm] $msg" }
function Have($cmd) { [bool](Get-Command $cmd -ErrorAction SilentlyContinue) }

function Add-McpEntry([string]$Path, [string]$TopKey, [hashtable]$Entry) {
    # Merge the $McpName entry under $TopKey in a JSON config, keeping the
    # rest of the file intact. Returns "added" or "exists".
    $data = [ordered]@{}
    if (Test-Path $Path) {
        $raw = (Get-Content $Path -Raw).Trim()
        if ($raw) { $data = $raw | ConvertFrom-Json }
    }
    if ($data -isnot [System.Collections.IDictionary]) {
        # PSCustomObject from ConvertFrom-Json — work on it directly.
        if (-not ($data.PSObject.Properties.Name -contains $TopKey)) {
            $data | Add-Member -MemberType NoteProperty -Name $TopKey -Value ([pscustomobject]@{})
        }
        if ($data.$TopKey.PSObject.Properties.Name -contains $McpName) { return "exists" }
        $data.$TopKey | Add-Member -MemberType NoteProperty -Name $McpName -Value ([pscustomobject]$Entry)
    } else {
        if (-not $data.Contains($TopKey)) { $data[$TopKey] = [ordered]@{} }
        if ($data[$TopKey].Contains($McpName)) { return "exists" }
        $data[$TopKey][$McpName] = $Entry
    }
    New-Item -ItemType Directory -Force -Path (Split-Path $Path) | Out-Null
    $data | ConvertTo-Json -Depth 16 | Set-Content -Path $Path -Encoding utf8
    return "added"
}

function Register-JsonAgent([string]$Label, [string]$Path, [string]$TopKey, [hashtable]$Entry) {
    if ((Add-McpEntry $Path $TopKey $Entry) -eq "added") {
        Log "${Label}: added '$McpName' -> $McpUrl to $Path"
    } else {
        Log "${Label}: MCP server '$McpName' already configured."
    }
}

function Install-Skill([string]$Dir) {
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    Invoke-WebRequest -Uri "$RawBase/skills/use-inxm-mcp/SKILL.md" -OutFile (Join-Path $Dir "SKILL.md")
}

# ------------------------------------------------------------------ install
$asset = "inxm-local-x86_64-pc-windows-msvc-setup.exe"
$url = if ($Version) {
    "https://github.com/$Repo/releases/download/v$($Version.TrimStart('v'))/$asset"
} else {
    "https://github.com/$Repo/releases/latest/download/$asset"
}

$setup = Join-Path $env:TEMP $asset
Log "Downloading $asset ..."
Invoke-WebRequest -Uri $url -OutFile $setup
# The setup is not code-signed yet; drop the mark-of-the-web so SmartScreen
# doesn't block the silent install.
Unblock-File $setup

Log "Running the installer (silent, per-user) ..."
Start-Process -FilePath $setup -ArgumentList "/VERYSILENT", "/NORESTART", "/CURRENTUSER" -Wait
Remove-Item $setup -ErrorAction SilentlyContinue
Log "Installed INXM Local."

# --------------------------------------------------------- agent integration
if ($Agents) {
    if (Have claude) { $Claude = $true }
    if ((Have codex) -or (Test-Path (Split-Path $Paths.CodexConfig))) { $Codex = $true }
    if ((Have gemini) -or (Test-Path (Split-Path $Paths.GeminiConfig))) { $Gemini = $true }
    if ((Have qwen) -or (Test-Path (Split-Path $Paths.QwenConfig))) { $Qwen = $true }
    if ((Have copilot) -or (Test-Path (Split-Path $Paths.CopilotConfig))) { $Copilot = $true }
    if (Test-Path $VSCodeUser) { $VSCode = $true }
    if (Test-Path (Split-Path $Paths.CursorConfig)) { $Cursor = $true }
    if (Test-Path (Split-Path $Paths.WindsurfConfig)) { $Windsurf = $true }
    if (Test-Path (Split-Path $Paths.ClineConfig)) { $Cline = $true }
    if (Test-Path (Split-Path $Paths.RooConfig)) { $Roo = $true }
    if ((Have opencode) -or (Test-Path (Split-Path $Paths.OpenCodeConfig))) { $OpenCode = $true }
    if ((Have goose) -or (Test-Path $Paths.GooseConfig)) { $Goose = $true }
    if (Have hermes) { $Hermes = $true }
    if ((Have pi) -or (Test-Path (Join-Path $env:USERPROFILE ".pi"))) { $Pi = $true }
    if ((Have zed) -or (Test-Path (Split-Path $Paths.ZedConfig))) { $Zed = $true }
    if (-not ($Claude -or $Codex -or $Gemini -or $Qwen -or $Copilot -or $VSCode -or $Cursor -or
              $Windsurf -or $Cline -or $Roo -or $OpenCode -or $Goose -or $Hermes -or $Pi -or $Zed)) {
        Log "No known agents found on this machine - skipping MCP registration."
    }
}

if ($Claude) {
    if (Have claude) {
        claude mcp get $McpName *> $null
        if ($LASTEXITCODE -eq 0) {
            Log "Claude Code: MCP server '$McpName' already registered."
        } else {
            claude mcp add --scope user --transport http $McpName $McpUrl
            Log "Claude Code: registered '$McpName' -> $McpUrl"
        }
        Install-Skill $Paths.ClaudeSkillDir
        Log "Claude Code: installed the 'use-inxm-mcp' skill."
    } else {
        Write-Warning "Claude Code CLI not found - install it, then rerun with -Claude."
    }
}

if ($Codex) {
    if ((Test-Path $Paths.CodexConfig) -and (Select-String -Path $Paths.CodexConfig -Pattern "mcp_servers.$McpName" -Quiet)) {
        Log "Codex: MCP server '$McpName' already configured."
    } else {
        New-Item -ItemType Directory -Force -Path (Split-Path $Paths.CodexConfig) | Out-Null
        Add-Content -Path $Paths.CodexConfig -Value "`n[mcp_servers.$McpName]`nurl = `"$McpUrl`""
        Log "Codex: added '$McpName' -> $McpUrl to ~\.codex\config.toml"
    }
}

if ($Gemini)   { Register-JsonAgent "Gemini CLI" $Paths.GeminiConfig "mcpServers" @{ httpUrl = $McpUrl } }
if ($Qwen)     { Register-JsonAgent "Qwen Code" $Paths.QwenConfig "mcpServers" @{ httpUrl = $McpUrl } }
if ($Copilot)  { Register-JsonAgent "Copilot CLI" $Paths.CopilotConfig "mcpServers" @{ type = "http"; url = $McpUrl } }
if ($VSCode)   { Register-JsonAgent "VS Code (Copilot)" $Paths.VSCodeConfig "servers" @{ type = "http"; url = $McpUrl } }
if ($Cursor)   { Register-JsonAgent "Cursor" $Paths.CursorConfig "mcpServers" @{ url = $McpUrl } }
if ($Windsurf) { Register-JsonAgent "Windsurf" $Paths.WindsurfConfig "mcpServers" @{ serverUrl = $McpUrl } }
if ($Cline)    { Register-JsonAgent "Cline" $Paths.ClineConfig "mcpServers" @{ type = "streamableHttp"; url = $McpUrl; disabled = $false; autoApprove = @() } }
if ($Roo)      { Register-JsonAgent "Roo Code" $Paths.RooConfig "mcpServers" @{ type = "streamable-http"; url = $McpUrl; disabled = $false; alwaysAllow = @() } }
if ($OpenCode) { Register-JsonAgent "OpenCode" $Paths.OpenCodeConfig "mcp" @{ type = "remote"; url = $McpUrl; enabled = $true } }

if ($Goose) {
    Write-Warning "Goose: add this to $($Paths.GooseConfig) under 'extensions:' manually:"
    Write-Warning "  ${McpName}: {enabled: true, name: ${McpName}, type: streamable_http, uri: $McpUrl}"
}

if ($Hermes) {
    if (Have hermes) {
        hermes mcp add $McpName --url $McpUrl
        if ($LASTEXITCODE -eq 0) {
            Log "Hermes: registered '$McpName' -> $McpUrl"
        } else {
            Write-Warning "Hermes: 'hermes mcp add' failed - add it to ~\.hermes\config.yaml manually (see docs/integration/hermes.md)."
        }
    } else {
        Write-Warning "Hermes CLI not found - see docs/integration/hermes.md for manual setup."
    }
}

if ($Pi) {
    # Pi has no native MCP client config; its skills directory is the
    # supported way to teach it the INXM endpoint.
    Install-Skill $Paths.PiSkillDir
    Log "Pi: installed the 'use-inxm-mcp' skill into ~\.pi\agent\skills."
}

if ($Zed) {
    Register-JsonAgent "Zed" $Paths.ZedConfig "context_servers" @{ url = $McpUrl }
    Install-Skill $Paths.ZedSkillDir
    Log "Zed: installed the 'use-inxm-mcp' skill into ~\.agents\skills (global, every project)."
}

Log "Done. Start INXM Local so agents can reach $McpUrl"
