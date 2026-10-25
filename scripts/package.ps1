# SPDX-License-Identifier: GPL-3.0-or-later
#
# Builds the release files from already built binaries:
#
#   thingsvu-tools-v<version>-windows-x64.zip    everything a user needs; install.ps1 sets it up
#   thingsvu-kicad-ai-pcm-v<version>.zip         the plugin for KiCad's Plugin and Content Manager
#   install.ps1                                  the same installer, to run without downloading the zip
#   SHA256SUMS.txt                               checksums of the files above
#
#   .\scripts\package.ps1                        uses target\release, writes to dist\
#   .\scripts\package.ps1 -BinDir target\debug -OutDir dist-test
#
# The version comes from Cargo.toml ([workspace.package]), the same value the release tag uses.
param(
    [string]$BinDir,
    [string]$OutDir,
    [string]$Version
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
if (-not $BinDir) { $BinDir = Join-Path $repo 'target\release' }
if (-not $OutDir) { $OutDir = Join-Path $repo 'dist' }

if (-not $Version) {
    $cargo = Get-Content (Join-Path $repo 'Cargo.toml') -Raw
    if ($cargo -notmatch '(?ms)\[workspace\.package\].*?^version\s*=\s*"([^"]+)"') { throw 'no version in Cargo.toml' }
    $Version = $Matches[1]
}

$mcp = Join-Path $BinDir 'kicad-ai-mcp.exe'
$plugin = Join-Path $BinDir 'kicad-ai-plugin.exe'
foreach ($f in @($mcp, $plugin)) {
    if (-not (Test-Path $f)) { throw "$f not found; build first: cargo build --release -p kicad-ai-mcp -p kicad-ai-plugin" }
}

New-Item -ItemType Directory -Force $OutDir | Out-Null

# Compress-Archive in Windows PowerShell 5.1 writes backslashes into entry names, which the zip
# format does not allow. This writes the entries with forward slashes.
function New-Zip([string]$Source, [string]$Destination) {
    Add-Type -AssemblyName System.IO.Compression
    $base = (Resolve-Path $Source).Path.TrimEnd('\')
    $stream = [IO.File]::Open($Destination, [IO.FileMode]::Create)
    try {
        $archive = New-Object IO.Compression.ZipArchive($stream, [IO.Compression.ZipArchiveMode]::Create)
        try {
            Get-ChildItem $base -Recurse -File | ForEach-Object {
                $name = $_.FullName.Substring($base.Length + 1).Replace('\', '/')
                $entry = $archive.CreateEntry($name, [IO.Compression.CompressionLevel]::Optimal)
                $in = [IO.File]::OpenRead($_.FullName)
                $out = $entry.Open()
                try { $in.CopyTo($out) } finally { $out.Dispose(); $in.Dispose() }
            }
        }
        finally { $archive.Dispose() }
    }
    finally { $stream.Dispose() }
}
$staging = Join-Path ([IO.Path]::GetTempPath()) ("thingsvu-package-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $staging | Out-Null
try {
    # --- The release zip -------------------------------------------------------------------
    $root = Join-Path $staging 'release'
    New-Item -ItemType Directory (Join-Path $root 'bin'), (Join-Path $root 'plugin\icons') | Out-Null
    Copy-Item $mcp, $plugin (Join-Path $root 'bin')
    Copy-Item (Join-Path $repo 'plugin-package\plugin.json') (Join-Path $root 'plugin')
    Copy-Item (Join-Path $repo 'plugin-package\icons\*.png') (Join-Path $root 'plugin\icons')
    Copy-Item (Join-Path $PSScriptRoot 'install.ps1'), (Join-Path $PSScriptRoot 'uninstall.ps1') $root
    Copy-Item (Join-Path $repo 'README.md'), (Join-Path $repo 'LICENSE') $root
    Set-Content (Join-Path $root 'VERSION.txt') $Version -Encoding ascii

    $zip = Join-Path $OutDir "thingsvu-tools-v$Version-windows-x64.zip"
    if (Test-Path $zip) { Remove-Item $zip }
    New-Zip $root $zip

    # --- The KiCad Plugin and Content Manager package -----------------------------------------
    # Layout and fields follow https://go.kicad.org/pcm/schemas/v1: metadata.json at the root and the
    # plugin files directly inside plugins\ (no second level).
    $pcm = Join-Path $staging 'pcm'
    New-Item -ItemType Directory (Join-Path $pcm 'plugins\icons') | Out-Null
    Copy-Item $plugin (Join-Path $pcm 'plugins')
    Copy-Item (Join-Path $repo 'plugin-package\plugin.json') (Join-Path $pcm 'plugins')
    Copy-Item (Join-Path $repo 'plugin-package\icons\*.png') (Join-Path $pcm 'plugins\icons')
    $metadata = [ordered]@{
        '$schema'        = 'https://go.kicad.org/pcm/schemas/v1'
        name             = 'KiCad AI'
        description      = 'Lets AI assistants (MCP) work with this KiCad editor: plan, apply, and verify schematic changes.'
        description_full = 'Adds a "KiCad AI: connect" toolbar button to the schematic editor. Pressing it makes the open editor available to the thingsvu-tools MCP server (kicad-ai-mcp), so an AI assistant can plan, apply, and verify schematic changes live, with normal undo. The plugin itself does nothing else. The MCP server is installed separately; see the project page.'
        identifier       = 'com.thingsvu.kicad-ai'
        type             = 'plugin'
        author           = [ordered]@{ name = 'thingsvu-tools contributors'; contact = [ordered]@{ web = 'https://github.com/bkrajendra/thingsvu-tools' } }
        license          = 'GPL-3.0'
        resources        = [ordered]@{ homepage = 'https://github.com/bkrajendra/thingsvu-tools' }
        versions         = @([ordered]@{
                version      = ($Version -replace '-.*$', '')
                status       = 'testing'
                kicad_version = '10.99'
                platforms    = @('windows')
                runtime      = 'ipc'
            })
    }
    # Compact the JSON: PowerShell 5.1 pads it with long runs of spaces.
    $json = ($metadata | ConvertTo-Json -Depth 8)
    [IO.File]::WriteAllText((Join-Path $pcm 'metadata.json'), $json, (New-Object Text.UTF8Encoding $false))

    $pcmZip = Join-Path $OutDir "thingsvu-kicad-ai-pcm-v$Version.zip"
    if (Test-Path $pcmZip) { Remove-Item $pcmZip }
    New-Zip $pcm $pcmZip
}
finally {
    Remove-Item -Recurse -Force $staging -ErrorAction SilentlyContinue
}

Copy-Item (Join-Path $PSScriptRoot 'install.ps1') (Join-Path $OutDir 'install.ps1') -Force

# --- Checksums --------------------------------------------------------------------------------
$sums = Get-ChildItem $OutDir -File | Where-Object { $_.Name -ne 'SHA256SUMS.txt' } | Sort-Object Name | ForEach-Object {
    '{0}  {1}' -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name
}
[IO.File]::WriteAllText((Join-Path $OutDir 'SHA256SUMS.txt'), (($sums -join "`n") + "`n"), (New-Object Text.UTF8Encoding $false))

Write-Host "Packaged thingsvu-tools $Version into $OutDir"
Get-ChildItem $OutDir -File | ForEach-Object { Write-Host ('  {0,-52} {1,10:N0} bytes' -f $_.Name, $_.Length) }
