# SPDX-License-Identifier: GPL-3.0-or-later
#
# Installs thingsvu-tools: the MCP server, the KiCad plugin, and (if you agree) the entry that
# makes your AI assistant start the server.
#
#   From the extracted release zip:   .\install.ps1
#   Without downloading anything:     irm https://raw.githubusercontent.com/bkrajendra/thingsvu-tools/main/scripts/install.ps1 | iex
#   Unattended:                       .\install.ps1 -Yes
#
# It never needs administrator rights: everything goes under your user profile.
#
#   -Version <x.y.z|latest>   which release to download when not run from the zip (default: newest)
#   -InstallDir <path>        where kicad-ai-mcp.exe goes (default: %LOCALAPPDATA%\thingsvu\kicad-ai\bin)
#   -KicadVersion <list>      plugin folders to install into, e.g. 11.0 (default: every KiCad 11 /
#                             10.99 user folder found under Documents\KiCad)
#   -NoPlugin                 skip the KiCad plugin
#   -SkipClients              do not touch Claude Code or Claude Desktop
#   -Clients <list>           which clients to offer: ClaudeCode, ClaudeDesktop (default: both)
#   -Yes                      answer yes to every question
param(
    [string]$Version = 'latest',
    [string]$InstallDir,
    [string[]]$KicadVersion,
    [switch]$NoPlugin,
    [switch]$SkipClients,
    [string[]]$Clients = @('ClaudeCode', 'ClaudeDesktop'),
    [switch]$Yes,
    [string]$DocumentsDir,
    [string]$ClaudeDesktopConfig,
    [string]$Repository = 'bkrajendra/thingsvu-tools'
)
$ErrorActionPreference = 'Stop'

if (-not $InstallDir) { $InstallDir = Join-Path $env:LOCALAPPDATA 'thingsvu\kicad-ai\bin' }
if (-not $DocumentsDir) { $DocumentsDir = [Environment]::GetFolderPath('MyDocuments') }
if (-not $ClaudeDesktopConfig) { $ClaudeDesktopConfig = Join-Path $env:APPDATA 'Claude\claude_desktop_config.json' }

# Runs a program and returns its exit code. Windows PowerShell 5.1 turns anything a program writes
# to stderr into an error when $ErrorActionPreference is 'Stop', even for a harmless message such as
# "No MCP server named ... in user scope"; only the exit code says whether it worked.
function Invoke-Native {
    param([string]$File, [string[]]$Arguments)
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $File @Arguments 2>&1 | Out-Null; return $LASTEXITCODE }
    finally { $ErrorActionPreference = $previous }
}

function Step($text) { Write-Host "`n== $text" -ForegroundColor Cyan }
function Ask($question) {
    if ($Yes) { return $true }
    try {
        $answer = Read-Host "$question [Y/n]"
        return ($answer -eq '' -or $answer -match '^[yY]')
    }
    catch { return $false }   # not interactive: do nothing without being told to
}

# --- 1. Find the files ----------------------------------------------------------------------
Step 'Finding the files'
$package = $null
if ($PSScriptRoot -and (Test-Path (Join-Path $PSScriptRoot 'bin\kicad-ai-mcp.exe'))) {
    $package = $PSScriptRoot
    Write-Host "Using the files next to this script ($package)"
}
else {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $headers = @{ 'User-Agent' = 'thingsvu-installer' }
    $api = "https://api.github.com/repos/$Repository/releases"
    if ($Version -eq 'latest') {
        # /releases/latest skips pre-releases, which every 0.x release is.
        $release = @(Invoke-RestMethod "${api}?per_page=10" -Headers $headers) | Where-Object { -not $_.draft } | Select-Object -First 1
    }
    else {
        $tag = if ($Version.StartsWith('v')) { $Version } else { "v$Version" }
        $release = Invoke-RestMethod "$api/tags/$tag" -Headers $headers
    }
    if (-not $release) {
        throw "No published release found in $Repository yet. A release appears a few minutes after a new version is pushed; try again shortly, or download the zip from https://github.com/$Repository/releases."
    }
    $zipAsset = $release.assets | Where-Object { $_.name -like '*-windows-x64.zip' } | Select-Object -First 1
    $sumsAsset = $release.assets | Where-Object { $_.name -eq 'SHA256SUMS.txt' } | Select-Object -First 1
    if (-not $zipAsset -or -not $sumsAsset) { throw "Release $($release.tag_name) has no Windows package." }

    $work = Join-Path ([IO.Path]::GetTempPath()) ("thingsvu-install-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory $work | Out-Null
    Write-Host "Downloading $($zipAsset.name) ($($release.tag_name))"
    $zipPath = Join-Path $work $zipAsset.name
    Invoke-WebRequest $zipAsset.browser_download_url -OutFile $zipPath -Headers $headers -UseBasicParsing
    Invoke-WebRequest $sumsAsset.browser_download_url -OutFile (Join-Path $work 'SHA256SUMS.txt') -Headers $headers -UseBasicParsing

    $expected = Get-Content (Join-Path $work 'SHA256SUMS.txt') | ForEach-Object {
        if ($_ -match '^([0-9a-fA-F]{64})\s+\*?(.+)$' -and $Matches[2] -eq $zipAsset.name) { $Matches[1].ToLower() }
    } | Select-Object -First 1
    $actual = (Get-FileHash $zipPath -Algorithm SHA256).Hash.ToLower()
    if (-not $expected -or $expected -ne $actual) { throw "The download does not match its published checksum; not installing." }
    Write-Host 'Checksum verified.'
    $package = Join-Path $work 'files'
    Expand-Archive $zipPath $package
}
$mcpSource = Join-Path $package 'bin\kicad-ai-mcp.exe'
$pluginExe = Join-Path $package 'bin\kicad-ai-plugin.exe'
$pluginFiles = Join-Path $package 'plugin'
foreach ($f in @($mcpSource, $pluginExe, (Join-Path $pluginFiles 'plugin.json'))) {
    if (-not (Test-Path $f)) { throw "The package is incomplete: $f is missing." }
}

# --- 2. The MCP server ----------------------------------------------------------------------
Step 'Installing the MCP server'
New-Item -ItemType Directory -Force $InstallDir | Out-Null
$mcp = Join-Path $InstallDir 'kicad-ai-mcp.exe'
try { Copy-Item $mcpSource $mcp -Force }
catch { throw "Cannot replace $mcp. An AI assistant is probably running it: close the assistant and run the installer again." }
Write-Host "Installed $mcp"
# Keep the uninstaller where it can be found later, even after a one-line install.
$uninstaller = Join-Path $package 'uninstall.ps1'
if (Test-Path $uninstaller) { Copy-Item $uninstaller (Join-Path (Split-Path $InstallDir) 'uninstall.ps1') -Force }

# --- 3. The KiCad plugin --------------------------------------------------------------------
$pluginTargets = @()
if (-not $NoPlugin) {
    Step 'Installing the KiCad plugin'
    $versions = $KicadVersion
    if (-not $versions) {
        $kicadDocs = Join-Path $DocumentsDir 'KiCad'
        $found = @(Get-ChildItem $kicadDocs -Directory -ErrorAction SilentlyContinue | Where-Object { $_.Name -match '^\d+\.\d+$' })
        $versions = @($found | Where-Object { [version]($_.Name + '.0') -ge [version]'10.99.0' } | ForEach-Object { $_.Name })
        if ($found | Where-Object { $_.Name -match '^10\.\d$' -and [version]($_.Name + '.0') -lt [version]'10.99.0' }) {
            Write-Host 'KiCad 10 found: it cannot run schematic-editor plugins, so no plugin is installed there.'
            Write-Host 'The MCP server still works with KiCad 10 through the schematic file (provider "file").'
        }
    }
    if (-not $versions) {
        Write-Host "No KiCad 11 user folder found under $DocumentsDir\KiCad."
        Write-Host 'Start KiCad 11 once, then run this installer again (or pass -KicadVersion 11.0).'
    }
    foreach ($v in $versions) {
        $target = Join-Path $DocumentsDir "KiCad\$v\plugins\com.thingsvu.kicad-ai"
        New-Item -ItemType Directory -Force (Join-Path $target 'icons') | Out-Null
        Copy-Item $pluginExe (Join-Path $target 'kicad-ai-plugin.exe') -Force
        Copy-Item (Join-Path $pluginFiles 'plugin.json') (Join-Path $target 'plugin.json') -Force
        Copy-Item (Join-Path $pluginFiles 'icons\*.png') (Join-Path $target 'icons') -Force
        Write-Host "Installed the plugin for KiCad $v ($target)"
        $pluginTargets += $v
    }
}

# --- 4. AI assistants -----------------------------------------------------------------------
$configured = @()
if (-not $SkipClients) {
    Step 'Connecting your AI assistant'
    if ($Clients -contains 'ClaudeCode') {
        $claude = Get-Command claude -ErrorAction SilentlyContinue
        if (-not $claude) {
            Write-Host 'Claude Code: not found on PATH (skipping).'
        }
        elseif (Ask 'Add the KiCad tools to Claude Code?') {
            # Replace an earlier entry; it is fine if there was none.
            $null = Invoke-Native 'claude' @('mcp', 'remove', 'kicad-ai', '--scope', 'user')
            $code = Invoke-Native 'claude' @('mcp', 'add', '--scope', 'user', 'kicad-ai', '--', $mcp)
            if ($code -eq 0) { $configured += 'Claude Code'; Write-Host 'Claude Code: added "kicad-ai".' }
            else { Write-Warning 'Claude Code: "claude mcp add" failed; add it by hand (see the README).' }
        }
    }
    if ($Clients -contains 'ClaudeDesktop') {
        if (-not (Test-Path (Split-Path $ClaudeDesktopConfig))) {
            Write-Host 'Claude Desktop: not found (skipping).'
        }
        elseif (Ask 'Add the KiCad tools to Claude Desktop?') {
            $raw = if (Test-Path $ClaudeDesktopConfig) { Get-Content $ClaudeDesktopConfig -Raw } else { '' }
            $cfg = $null
            $valid = $true
            if ($raw -and $raw.Trim()) {
                try { $cfg = $raw | ConvertFrom-Json }
                catch { $valid = $false; Write-Warning "Claude Desktop: $ClaudeDesktopConfig is not valid JSON; leaving it alone." }
            }
            if ($valid) {
                if ($null -eq $cfg) { $cfg = [pscustomobject]@{} }
                if (-not ($cfg.PSObject.Properties.Name -contains 'mcpServers')) {
                    $cfg | Add-Member -NotePropertyName mcpServers -NotePropertyValue ([pscustomobject]@{})
                }
                $entry = [pscustomobject]@{ command = $mcp; args = @() }
                if ($cfg.mcpServers.PSObject.Properties.Name -contains 'kicad-ai') { $cfg.mcpServers.'kicad-ai' = $entry }
                else { $cfg.mcpServers | Add-Member -NotePropertyName 'kicad-ai' -NotePropertyValue $entry }
                if (Test-Path $ClaudeDesktopConfig) {
                    $backup = "$ClaudeDesktopConfig.bak-" + (Get-Date -Format 'yyyyMMddHHmmss')
                    Copy-Item $ClaudeDesktopConfig $backup
                    Write-Host "Claude Desktop: kept your old settings in $backup"
                }
                [IO.File]::WriteAllText($ClaudeDesktopConfig, ($cfg | ConvertTo-Json -Depth 20), (New-Object Text.UTF8Encoding $false))
                $configured += 'Claude Desktop'
                Write-Host 'Claude Desktop: added "kicad-ai".'
            }
        }
    }
}

# --- 5. What next ---------------------------------------------------------------------------
Step 'Done'
Write-Host "MCP server:  $mcp"
if ($pluginTargets) { Write-Host ('KiCad plugin: installed for KiCad ' + ($pluginTargets -join ', ')) }
if ($configured) { Write-Host ('Connected:   ' + ($configured -join ', ') + ' (restart it to pick up the new tools)') }
Write-Host @"

Next steps
  1. Other AI assistants: run this server over stdio. The command is
       $mcp
     For VS Code, add it to .vscode/mcp.json:
       { "servers": { "kicad-ai": { "type": "stdio", "command": "$($mcp -replace '\\', '/')" } } }
  2. KiCad 11 (live editing): Preferences > Plugins > enable the KiCad API, restart KiCad, open the
     Schematic Editor, and press the "KiCad AI: connect" toolbar button.
  3. KiCad 10 or no KiCad running: ask your assistant to open the .kicad_sch file with the "file" provider.
  4. Try it: "Open <your project>.kicad_sch and call session_status".
"@
