<#
.SYNOPSIS
    Configures Claude Desktop, Antigravity, Cursor, and Codex to use rusty_remind_me as an MCP memory backend.
.DESCRIPTION
    This script locates rusty-remind-me.exe and safely merges the "rusty-remind-me" server entry into:
      - Claude Desktop (%APPDATA%\Claude\claude_desktop_config.json)
      - Antigravity (%USERPROFILE%\.gemini\antigravity\mcp_config.json)
      - Cursor (%USERPROFILE%\.cursor\mcp.json)
      - Codex / Generic (%USERPROFILE%\.mcp\config.json)
#>

[CmdletBinding()]
param (
    [string]$ExecutablePath = "",
    [string]$DbPath = "$env:USERPROFILE\.remind-me\memory.db"
)

# 1. Resolve Executable Path
if ([string]::IsNullOrWhiteSpace($ExecutablePath)) {
    # The Cargo workspace root (where Cargo.lock and target\ live) at or
    # above this product's directory -- since the move into the rusty_mill
    # monorepo that is the monorepo root, not the product directory itself.
    $WorkspaceRoot = Split-Path -Parent $PSScriptRoot
    while ($WorkspaceRoot -and -not (Test-Path (Join-Path $WorkspaceRoot "Cargo.lock"))) {
        $WorkspaceRoot = Split-Path -Parent $WorkspaceRoot
    }
    if (-not $WorkspaceRoot) {
        Write-Error "No Cargo.lock found at or above $PSScriptRoot"
        exit 1
    }
    $ReleaseExe = Join-Path $WorkspaceRoot "target\release\rusty-remind-me.exe"
    $DebugExe   = Join-Path $WorkspaceRoot "target\debug\rusty-remind-me.exe"

    if (Test-Path $ReleaseExe) {
        $ExecutablePath = $ReleaseExe
    } elseif (Test-Path $DebugExe) {
        $ExecutablePath = $DebugExe
    } else {
        Write-Host "Building rusty-remind-me release binary..." -ForegroundColor Yellow
        Push-Location $WorkspaceRoot
        cargo build --release -p rusty-remind-me
        Pop-Location
        $ExecutablePath = $ReleaseExe
    }
}

if (-not (Test-Path $ExecutablePath)) {
    Write-Error "Could not find rusty-remind-me.exe at '$ExecutablePath'"
    exit 1
}

Write-Host "Using Executable: $ExecutablePath" -ForegroundColor Green
Write-Host "Using Database:   $DbPath" -ForegroundColor Green

# Ensure DB folder exists
$DbDir = [System.IO.Path]::GetDirectoryName($DbPath)
if (-not (Test-Path $DbDir)) {
    New-Item -ItemType Directory -Path $DbDir -Force | Out-Null
}

# 2. Server Definition Template
$ServerConfig = [ordered]@{
    "command" = $ExecutablePath
    "args"    = @("server")
    "env"     = [ordered]@{
        "REMIND_ME_DB_PATH" = $DbPath
    }
}

function Merge-McpConfig {
    param (
        [string]$ConfigPath,
        [string]$TargetName
    )

    $ConfigDir = [System.IO.Path]::GetDirectoryName($ConfigPath)
    if (-not (Test-Path $ConfigDir)) {
        New-Item -ItemType Directory -Path $ConfigDir -Force | Out-Null
    }

    $JsonObj = @{ "mcpServers" = @{} }

    if (Test-Path $ConfigPath) {
        try {
            $RawText = Get-Content $ConfigPath -Raw -ErrorAction Stop
            if (-not [string]::IsNullOrWhiteSpace($RawText)) {
                $Parsed = $RawText | ConvertFrom-Json
                if ($Parsed.PSObject.Properties['mcpServers']) {
                    $JsonObj = $Parsed
                } else {
                    $JsonObj = [ordered]@{ "mcpServers" = $Parsed }
                }
            }
        } catch {
            Write-Warning "Failed to parse existing config at '$ConfigPath'. Creating backup."
            Copy-Item $ConfigPath "$ConfigPath.bak" -Force
        }
    }

    # Add or update rusty-remind-me server
    if (-not $JsonObj.mcpServers) {
        $JsonObj | Add-Member -MemberType NoteProperty -Name "mcpServers" -Value @{}
    }
    
    $JsonObj.mcpServers | Add-Member -MemberType NoteProperty -Name "rusty-remind-me" -Value $ServerConfig -Force

    $UpdatedJson = $JsonObj | ConvertTo-Json -Depth 10
    Set-Content -Path $ConfigPath -Value $UpdatedJson -Encoding UTF8
    Write-Host "Successfully configured $TargetName at '$ConfigPath'" -ForegroundColor Green
}

# 3. Target Clients
$ClaudeConfigPath      = "$env:APPDATA\Claude\claude_desktop_config.json"
$AntigravityConfigPath = "$env:USERPROFILE\.gemini\antigravity\mcp_config.json"
$CursorConfigPath      = "$env:USERPROFILE\.cursor\mcp.json"
$CodexConfigPath       = "$env:USERPROFILE\.mcp\config.json"

Merge-McpConfig -ConfigPath $ClaudeConfigPath      -TargetName "Claude Desktop"
Merge-McpConfig -ConfigPath $AntigravityConfigPath -TargetName "Antigravity"
Merge-McpConfig -ConfigPath $CursorConfigPath      -TargetName "Cursor"
Merge-McpConfig -ConfigPath $CodexConfigPath       -TargetName "Codex / Generic MCP"

Write-Host "`nSetup complete! Restart your client (Claude Desktop, Antigravity, Cursor, Codex) to activate rusty-remind-me memory backend." -ForegroundColor Cyan
