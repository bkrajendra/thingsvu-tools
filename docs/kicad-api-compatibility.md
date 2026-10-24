# KiCad API compatibility record

Observed facts per pinned KiCad build. Update this file whenever a pinned build changes; re-run the Phase 0 probes first.

## Pinned builds

| Tier | Build | Install path | `kicad-cli` | API server setting |
|---|---|---|---|---|
| 1 (KiCad 11) | `10.99.0-5896-gd033635628`, release build, 23 Oct 2026 | `D:\Program Files\KiCad\10.99` | has `api-server` | `api.enable_server = true` |
| 2 (KiCad 10) | `10.0.7`, release build, 6 Oct 2026 (`KICAD_IPC_API=ON`) | `C:\Program Files\KiCad\10.0` | no `api-server` | enabled by user, 24 Oct 2026 |

Platform: Windows 11 Pro 26H2 (build 26300), x64.

Environment: machine-level `KICAD_HOME = D:\Program Files\KiCad\10.99`; `%KICAD_HOME%\bin` on machine `Path` (REG_EXPAND_SZ). Already-running processes must be restarted to see it.

User documents (plugin directories): `C:\Users\bkraj\OneDrive\Documents\KiCad\10.0`, `...\10.99`.

## Protos

| Tier | Source | Commit | SHA-256 per file |
|---|---|---|---|
| 1 | `gitlab.com/kicad/code/kicad` `api/proto` | `d033635628` | _record at vendoring_ |

License: KiCad protos carry GPL-3.0-or-later headers; this project is GPL-3.0-or-later (decided 24 Oct 2026).

## `kicad-cli` differences observed

| Command | 10.0.7 | 10.99 nightly |
|---|---|---|
| Top level | `fp`, `jobset`, `pcb`, `sch`, `sym`, `version` | adds `api-server`, `gerber`, `import`, `mergetool`, `project` |
| `sch` | `erc`, `export`, `upgrade` | adds `diff`, `downgrade`, `import` |
| `sch export` | `bom`, `dxf`, `hpgl`, `netlist`, `pdf`, `ps`, `python-bom`, `svg` | adds `png` |
| `sch erc` | `--format json\|report`, severity flags, `--exit-code-violations` | same |
| `api-server` | — | `kicad-cli api-server [--socket SOCKET_PATH] [PROJECT_OR_FILE]` |

## Probe results

| Probe | Nightly headless | Nightly GUI | 10.0.7 GUI |
|---|---|---|---|
| `api-server` starts with `--socket <path>` and no file | Process stays alive (24 Oct) | — | — |
| `api-server` with a hand-written minimal `.kicad_sch` | `Failed to load schematic` — use KiCad-authored fixtures | — | — |
| Endpoint visible in `\\.\pipe\` listing | **Not found** with `--socket <path>`; resolve via NNG client | _todo_ | _todo_ |
| NNG handshake, `GetVersion`, `Ping` | _todo_ | _todo_ | _todo_ |
| Token behavior (empty / pinned / mismatch) | _todo_ | _todo_ | _todo_ |
| `GetOpenDocuments` | _todo_ | _todo_ | _todo_ |
| `GetSchematicHierarchy`, `GetSchematicNetlist` | _todo_ | _todo_ | _todo (expect `AS_UNHANDLED`)_ |
| `PlaceSymbolFromLibrary` | _todo_ | _todo_ | _todo (expect unsupported)_ |
| `CreateItems` wire + label, commit | _todo_ | _todo_ | _todo (expect unsupported)_ |
| `SaveDocument`, `RevertDocument` | _todo_ | _todo_ | _todo_ |
| Schematic ERC/export job over IPC | _todo_ | _todo_ | — |
| v11 generated protos decode v10 replies | — | — | _todo_ |
| Exec plugin lifetime, schematic scope | — | _todo_ | — |
