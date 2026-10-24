# SPDX-License-Identifier: GPL-3.0-or-later
#
# Removes the KiCad AI executable plugin from a KiCad plugin folder and clears registrations.
param(
    [string]$KicadVersion = '10.99'
)
$ErrorActionPreference = 'Stop'

$documents = [Environment]::GetFolderPath('MyDocuments')
$target = Join-Path $documents "KiCad\$KicadVersion\plugins\com.thingsvu.kicad-ai"
if (Test-Path $target) {
    Remove-Item -Recurse -Force $target
    Write-Host "Removed $target"
} else {
    Write-Host "Not installed in $target"
}

$registry = Join-Path $env:LOCALAPPDATA 'thingsvu\kicad-ai\instances'
if (Test-Path $registry) {
    Remove-Item -Recurse -Force $registry
    Write-Host 'Cleared KiCad AI editor registrations'
}
