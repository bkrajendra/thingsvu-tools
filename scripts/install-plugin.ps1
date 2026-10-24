# SPDX-License-Identifier: GPL-3.0-or-later
#
# Installs the KiCad AI executable plugin into a KiCad 11 (or 10.99 nightly) plugin folder.
#
#   .\scripts\install-plugin.ps1                 # install the already built release binary
#   .\scripts\install-plugin.ps1 -Build          # build it first
#   .\scripts\install-plugin.ps1 -KicadVersion 11.0
#
# Restart KiCad (or use Preferences > Plugins > Refresh) afterwards. KiCad 10 cannot run
# schematic-editor plugins, so installing there is refused.
param(
    [string]$KicadVersion = '10.99',
    [switch]$Build
)
$ErrorActionPreference = 'Stop'

if ([version]($KicadVersion + '.0') -lt [version]'10.99.0') {
    throw "KiCad $KicadVersion does not support schematic-editor plugins; use KiCad 11 or a 10.99 nightly."
}

$repo = Split-Path -Parent $PSScriptRoot
if ($Build) {
    & cargo build --release -p kicad-ai-plugin --manifest-path (Join-Path $repo 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
}
$binary = Join-Path $repo 'target\release\kicad-ai-plugin.exe'
if (-not (Test-Path $binary)) { throw "$binary not found; run with -Build" }

# KiCad keeps plugins under the user's Documents folder, which may be redirected (e.g. OneDrive).
$documents = [Environment]::GetFolderPath('MyDocuments')
$target = Join-Path $documents "KiCad\$KicadVersion\plugins\com.thingsvu.kicad-ai"
New-Item -ItemType Directory -Force (Join-Path $target 'icons') | Out-Null

Copy-Item $binary (Join-Path $target 'kicad-ai-plugin.exe') -Force
Copy-Item (Join-Path $repo 'plugin-package\icons\*.png') (Join-Path $target 'icons') -Force

# KiCad only accepts an entrypoint relative to the plugin folder (absolute paths are skipped).
Copy-Item (Join-Path $repo 'plugin-package\plugin.json') (Join-Path $target 'plugin.json') -Force

Write-Host "Installed KiCad AI plugin to $target"
Write-Host 'Restart KiCad, open your project in the Schematic Editor, and press the "KiCad AI: connect" toolbar button.'
