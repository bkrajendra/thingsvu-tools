# SPDX-License-Identifier: GPL-3.0-or-later
#
# Smoke test for the release package and its installer, run in a sandbox (a temporary "Documents",
# "AppData", and install folder), so nothing on the machine is touched.
#
#   .\scripts\package.ps1 -BinDir target\debug -OutDir dist-test
#   .\scripts\test-installer.ps1 -DistDir dist-test
param([Parameter(Mandatory)][string]$DistDir)
$ErrorActionPreference = 'Stop'

function Check($condition, $message) {
    if (-not $condition) { throw "FAILED: $message" }
    Write-Host "  ok  $message"
}
function Run-Script($path, [string[]]$arguments) {
    # A fresh Windows PowerShell, as a user's would be.
    $output = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $path @arguments 2>&1
    if ($LASTEXITCODE -ne 0) { throw "$path failed:`n$($output -join "`n")" }
    return ($output -join "`n")
}

$zip = Get-ChildItem $DistDir -Filter 'thingsvu-tools-v*-windows-x64.zip' | Select-Object -First 1
if (-not $zip) { throw "no release zip in $DistDir" }
$sandbox = Join-Path ([IO.Path]::GetTempPath()) ("thingsvu-installer-test-" + [guid]::NewGuid().ToString('N'))
$documents = Join-Path $sandbox 'Documents'
$data = Join-Path $sandbox 'data'
$package = Join-Path $sandbox 'package'
$desktop = Join-Path $sandbox 'Claude\claude_desktop_config.json'
New-Item -ItemType Directory $documents, "$documents\KiCad\10.99", "$documents\KiCad\10.0", (Split-Path $desktop) | Out-Null
Expand-Archive $zip.FullName $package
$version = (Get-Content (Join-Path $package 'VERSION.txt')).Trim()
$other = '{ "mcpServers": { "other": { "command": "C:\\other.exe", "args": ["--x"] } }, "theme": "dark" }'
[IO.File]::WriteAllText($desktop, $other)

$installArgs = @('-Yes', '-InstallDir', "$data\bin", '-DocumentsDir', $documents, '-Clients', 'ClaudeDesktop', '-ClaudeDesktopConfig', $desktop)
try {
    Write-Host 'Install'
    $text = Run-Script (Join-Path $package 'install.ps1') $installArgs
    $exe = "$data\bin\kicad-ai-mcp.exe"
    Check (Test-Path $exe) 'the MCP server is installed'
    # --version prints on stderr, which Windows PowerShell 5.1 treats as an error when told to stop.
    $reported = & { $ErrorActionPreference = 'Continue'; & $exe --version 2>&1 | Out-String }
    Check ($reported -match [regex]::Escape($version)) "it reports version $version"
    Check (Test-Path "$data\uninstall.ps1") 'the uninstaller is kept next to the server'
    Check (Test-Path "$documents\KiCad\10.99\plugins\com.thingsvu.kicad-ai\kicad-ai-plugin.exe") 'the plugin is installed for 10.99'
    Check (Test-Path "$documents\KiCad\10.99\plugins\com.thingsvu.kicad-ai\plugin.json") 'with its manifest next to the executable'
    Check (Test-Path "$documents\KiCad\10.99\plugins\com.thingsvu.kicad-ai\icons\kicad-ai-light-24.png") 'and its icons'
    Check (-not (Test-Path "$documents\KiCad\10.0\plugins")) 'nothing is installed for KiCad 10, which cannot run the plugin'
    Check ($text -match 'KiCad 10 found') 'and the user is told why'
    $cfg = Get-Content $desktop -Raw | ConvertFrom-Json
    Check ($cfg.mcpServers.'kicad-ai'.command -eq $exe) 'Claude Desktop starts the installed server'
    Check ($cfg.mcpServers.other.command -eq 'C:\other.exe' -and $cfg.theme -eq 'dark') "the user's other settings are kept"
    Check (@(Get-ChildItem (Split-Path $desktop) -Filter '*.bak-*').Count -eq 1) 'the old settings are backed up'

    Write-Host 'Install again'
    Run-Script (Join-Path $package 'install.ps1') $installArgs | Out-Null
    $cfg = Get-Content $desktop -Raw | ConvertFrom-Json
    Check (@($cfg.mcpServers.PSObject.Properties).Count -eq 2) 'a second run does not duplicate the entry'

    Write-Host 'A settings file that is not JSON is left alone'
    [IO.File]::WriteAllText($desktop, 'not json {')
    $text = Run-Script (Join-Path $package 'install.ps1') $installArgs
    Check ((Get-Content $desktop -Raw) -eq 'not json {') 'it is not overwritten'
    [IO.File]::WriteAllText($desktop, ($cfg | ConvertTo-Json -Depth 20))

    Write-Host 'Claude Code (a stand-in that behaves like the real one)'
    # The real `claude mcp remove` writes to stderr and fails when there is nothing to remove; that
    # must not stop the installer (Windows PowerShell 5.1 turns stderr into an error).
    $fake = Join-Path $sandbox 'fakeclaude'
    New-Item -ItemType Directory $fake | Out-Null
    $log = Join-Path $fake 'calls.txt'
    $cmd = @(
        '@echo off',
        'echo %* >> "%~dp0calls.txt"',
        'if "%2"=="remove" (',
        '  echo No MCP server named "kicad-ai" in user scope 1>&2',
        '  exit /b 1',
        ')',
        'exit /b 0'
    )
    Set-Content (Join-Path $fake 'claude.cmd') $cmd -Encoding ascii
    $savedPath = $env:PATH
    $env:PATH = "$fake;$env:PATH"
    try {
        $text = Run-Script (Join-Path $package 'install.ps1') @('-Yes', '-InstallDir', "$data\bin", '-DocumentsDir', $documents, '-Clients', 'ClaudeCode')
        Check ($text -match 'Claude Code: added') 'the installer carries on after "remove" fails and adds the entry'
        $calls = Get-Content $log -Raw
        Check ($calls -match 'mcp add --scope user kicad-ai -- ') 'with the right command'
        Check ($calls -match [regex]::Escape("$data\bin\kicad-ai-mcp.exe")) 'pointing at the installed server'
        Remove-Item $log
        Run-Script (Join-Path $package 'uninstall.ps1') @('-Yes', '-DataDir', "$sandbox\nothing", '-DocumentsDir', $documents, '-Clients', 'ClaudeCode') | Out-Null
        Check ((Get-Content $log -Raw) -match 'mcp remove kicad-ai --scope user') 'the uninstaller removes it, and an empty "remove" is not an error'
    }
    finally { $env:PATH = $savedPath }

    Write-Host 'Uninstall'
    Run-Script (Join-Path $package 'uninstall.ps1') @('-Yes', '-DataDir', $data, '-DocumentsDir', $documents, '-Clients', 'ClaudeDesktop', '-ClaudeDesktopConfig', $desktop) | Out-Null
    Check (-not (Test-Path $data)) 'the server and its data are removed'
    Check (-not (Test-Path "$documents\KiCad\10.99\plugins\com.thingsvu.kicad-ai")) 'the plugin is removed'
    $cfg = Get-Content $desktop -Raw | ConvertFrom-Json
    Check (-not ($cfg.mcpServers.PSObject.Properties.Name -contains 'kicad-ai')) 'the Claude Desktop entry is removed'
    Check ($cfg.mcpServers.other.command -eq 'C:\other.exe') "the user's other servers stay"
    Check (Test-Path "$documents\KiCad\10.99") "KiCad's own folders stay"
    Write-Host 'Installer smoke test passed.'
}
finally {
    Remove-Item -Recurse -Force $sandbox -ErrorAction SilentlyContinue
}
