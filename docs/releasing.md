# Releasing

A release is made by changing the version in `Cargo.toml` and pushing to `main`. Nothing else is needed.

## How it works

The release workflow ([.github/workflows/release.yml](../.github/workflows/release.yml)) runs on every push to `main` and reads `version` from `[workspace.package]` in `Cargo.toml`. The git tag is always `v<version>`.

- **The tag already exists:** nothing happens. Ordinary pushes do not release.
- **The tag is new:** the workflow runs the tests, builds the Windows and Linux binaries, builds the packages, smoke-tests the installer in a sandbox, then creates the tag and the GitHub release with all files attached and a `SHA256SUMS.txt`.
- A version below 1.0, or with a suffix (`0.2.0-rc.1`), is published as a **pre-release**.

The tag is created by the release itself, on the commit that was pushed. There is no separate step to tag by hand, and a tag that exists is never moved or recreated.

## To release

1. Pick the next version (semantic versioning; the project is `0.x` until it is declared stable).
2. Change `version` under `[workspace.package]` in `Cargo.toml`. All crates share it.
3. Run `cargo build` so `Cargo.lock` follows, and commit both.
4. Update the Phase notes in [tasks.md](tasks.md) if the release closes a phase.
5. Push to `main`. Watch **Actions > Release**.

If a run fails after building but before publishing, re-run it (**Actions > Release > Run workflow**); no tag was created yet. If the release exists but a file is wrong, fix the problem and release the next version: published versions are not replaced.

## What is published

| File | What it is |
|---|---|
| `thingsvu-tools-v<version>-windows-x64.zip` | `bin\kicad-ai-mcp.exe`, `bin\kicad-ai-plugin.exe`, the plugin manifest and icons, `install.ps1`, `uninstall.ps1`, README, LICENSE, `VERSION.txt` |
| `install.ps1` | The installer on its own: downloads the newest release, checks it against `SHA256SUMS.txt`, installs |
| `thingsvu-kicad-ai-pcm-v<version>.zip` | The plugin for KiCad's Plugin and Content Manager (**Install from File**); metadata follows the [PCM schema](https://go.kicad.org/pcm/schemas/v1). Experimental: see below |
| `thingsvu-tools-v<version>-linux-x64.tar.gz` | The MCP server for Linux (file and headless providers; the KiCad plugin is Windows-only for now) |
| `SHA256SUMS.txt` | SHA-256 of every file above |

## Building the packages locally

```powershell
cargo build --release -p kicad-ai-mcp -p kicad-ai-plugin
.\scripts\package.ps1                              # writes .\dist
.\scripts\test-installer.ps1 -DistDir .\dist       # installs into a sandbox and removes it again
```

`package.ps1` takes the version from `Cargo.toml`, the same value the release uses.

## Known gaps

- **The PCM package has not been installed through KiCad's Plugin and Content Manager.** Its layout and metadata follow the published schema, but where KiCad 11 looks for an API plugin installed that way has not been confirmed. The installer's plugin folder (`Documents\KiCad\<version>\plugins`) is the tested path.
- **Binaries are not code-signed.** Windows SmartScreen may warn when you run them; checking the SHA-256 against `SHA256SUMS.txt` shows the download is the published one.
- **Windows and Linux x64 only.** macOS and Linux plugin builds need the plugin's dialog and paths ported.
