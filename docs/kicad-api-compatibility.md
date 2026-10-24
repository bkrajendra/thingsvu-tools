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
| 1 | `gitlab.com/kicad/code/kicad` `api/proto` (24 files, vendored in `crates/kicad-ipc/proto/`) | `d033635628` | `crates/kicad-ipc/proto/SHA256SUMS` (manifest SHA-256 `033e0bbc…5db8`) |

License: 20 proto files are GPL-3.0-or-later and 4 are MIT (library and variant protos); this project is GPL-3.0-or-later (decided 24 Oct 2026).

## `kicad-cli` differences observed

| Command | 10.0.7 | 10.99 nightly |
|---|---|---|
| Top level | `fp`, `jobset`, `pcb`, `sch`, `sym`, `version` | adds `api-server`, `gerber`, `import`, `mergetool`, `project` |
| `sch` | `erc`, `export`, `upgrade` | adds `diff`, `downgrade`, `import` |
| `sch export` | `bom`, `dxf`, `hpgl`, `netlist`, `pdf`, `ps`, `python-bom`, `svg` | adds `png` |
| `sch erc` | `--format json\|report`, severity flags, `--exit-code-violations` | same |
| `api-server` | — | `kicad-cli api-server [--socket SOCKET_PATH] [PROJECT_OR_FILE]` |

## Probe results

Probe tool: `scripts/probe-kicad.ps1`. Fixtures produced along the way: `tests/fixtures/v11/`.

| Probe | Nightly headless | Nightly GUI | 10.0.7 GUI |
|---|---|---|---|
| `api-server --socket <path>` | ✅ honored: pipe `\\.\pipe\<path>` (verified with `…\run\k.sock`). One earlier attempt with `…\run\api.sock` never became reachable; cause not yet known. With stdout redirected the server prints nothing (no "listening" line), so readiness must be detected by connect-and-`Ping` polling | — | — |
| `api-server` with a hand-written minimal `.kicad_sch` | `Failed to load schematic` — use KiCad-authored fixtures | — | — |
| Default endpoint (no `--socket`) | `ipc://C:\Users\<user>\AppData\Local\Temp\kicad\api.sock` → named pipe `\\.\pipe\C:\Users\<user>\AppData\Local\Temp\kicad\api.sock` (24 Oct) | _todo_ | _todo_ |
| Endpoint visible in `\\.\pipe\` listing | Not listed by `Directory.GetFiles('\\.\pipe\')` — the name contains backslashes. Discovery must derive the name from the socket path, not enumerate pipes | _todo_ | _todo_ |
| NNG handshake (SP header, REQ0 ↔ REP0) | ✅ peer replies `00 53 50 00 00 31 00 00` (REP0) (24 Oct) | _todo_ | _todo_ |
| NNG ipc framing | ✅ `0x01` + u64 big-endian length + body; REQ body prefixed with 4-byte request id (high bit set), echoed in reply | _todo_ | _todo_ |
| `GetVersion` | ✅ `AS_OK`; `major=10 minor=99 patch=0`, `full_version="10.99.0-5896-gd033635628"` | _todo_ | _todo_ |
| `Ping` | ✅ `AS_OK`; reply `google.protobuf.Empty` | _todo_ | _todo_ |
| Token behavior (empty / pinned / mismatch) | Empty token accepted. Reply header carries instance token (UUID form, stable across calls and connections). Pinned / mismatch: _todo_ | _todo_ | _todo_ |
| `GetOpenDocuments` (SCHEMATIC / PCB / PROJECT) | Without a loaded file: `AS_UNHANDLED` for all types. With `api-server <file>.kicad_sch`: SCHEMATIC ✅ returns `DocumentSpecifier{type=SCHEMATIC, project{name, path}}` (no sheet path); PCB still `AS_UNHANDLED` (24 Oct) | _todo_ | _todo_ |
| Instance token | Differs per server instance (`2757…` → `2e39…` after restart): usable to pin a session | _todo_ | _todo_ |
| `GetSchematicHierarchy` | ✅ root `SheetInstance{path=[root KIID], "/", name, filename, page "1"}`. Root KIID equals the file's top-level `(uuid …)` — file UUIDs and IPC KIIDs are the same identity | _todo_ | _todo (expect `AS_UNHANDLED`)_ |
| `GetSchematicNetlist` | ✅ empty schematic → no nets. After placing R1: nets `unconnected-(R1-Pad1)`, `unconnected-(R1-Pad2)`, each listing the **pin KIIDs** per sheet (not symbol/pin-number pairs — map via the symbol's pins) | _todo_ | _todo_ |
| `PlaceSymbolFromLibrary` before library load | `AS_BAD_REQUEST` "symbol 'Device:R' not found" — headless does not preload libraries | — | — |
| `LoadAllLibraries{LT_SYMBOL}` | ✅ `LibraryCommandStatus{LCS_OK}`, returns immediately (async); 223 symbol libraries in the table. `GetLibraryStatuses` reported `Device` as status 1 (`LLS_INVALID`) yet placement succeeded 1 s later — re-check status decoding/semantics | no-op in GUI per proto docs | — |
| `PlaceSymbolFromLibrary` (Device:R, 25.4 mm, 25.4 mm, ref R1) | ✅ returns `PlaceFromLibraryResponse` with a full `SchematicSymbolInstance`: new KIID, sheet path, position in nm, library symbol (pins 1/2 at local y = ∓3.81 mm with their own KIIDs), fields (Reference = "R1", Value = "R", Footprint, Datasheet, Description). In-memory only; file on disk unchanged | _todo_ | _todo (expect unsupported)_ |
| Unknown command type | `AS_UNHANDLED` with message "no handler available for request of type …" | _todo_ | _todo_ |
| Server robustness | Repeated connect/handshake/close cycles left the server responsive | _todo_ | _todo_ |
| `UpdateItems` (SchematicSymbolInstance with edited `value_field.text`) | ✅ `ISC_OK` per item; returned instances show Value = "10k". Full-object replace from the placed instance's bytes (no field mask) | _todo_ | _todo_ |
| `CreateItems` (SchematicLine wire, 3× LocalLabel) | ✅ `ISC_OK` per item; each returned with a new KIID and default stroke/text attributes | _todo_ | _todo (expect unsupported)_ |
| `BeginCommit` / `EndCommit` | ❌ `BeginCommit` → `AS_UNHANDLED` in headless; edits apply immediately per request. Re-test in GUI (undo grouping) | _todo_ | _todo_ |
| Connectivity read-back after edits | ✅ `GetSchematicNetlist`: `/INPUT` = {R1.1, label}; `/MID` = {wire, R1.2, label, R2.1}; `/OUTPUT` = {R2.2, label}. Labels placed exactly on pin endpoints connect | _todo_ | _todo_ |
| `SaveCopyOfDocument` (`include_project=true`) | ✅ writes `.kicad_sch`, `.kicad_pro`, `.kicad_prl` to the target; loaded document unchanged. Target directory must exist ("could not be opened" otherwise) | _todo_ | _todo_ |
| `SaveDocument`, `RevertDocument` | _todo_ | _todo_ | _todo_ |
| Saved copy verified by `kicad-cli` 10.99 | ✅ netlist matches fixture intent; ERC 2 warnings (`isolated_pin_label`, expected) | — | — |
| KiCad 11 file read by 10.0.7 `kicad-cli` | ❌ "Failed to load schematic" (exit 3), as expected from format `20261015` | — | — |
| Native Rust transport + client (`crates/kicad-ipc`, `examples/probe.rs`) | ✅ full two-resistor demo from a fresh empty fixture: typed protobuf messages, token pinned from first reply, pin KIIDs mapped to `R1.1` etc.; netlist and ERC of the saved copy identical to the PowerShell run | _todo_ | _todo_ |
| Readiness by bare pipe connect/close (no NNG handshake) | Server stayed healthy and served the probe immediately after | _todo_ | _todo_ |
| Schematic ERC/export job over IPC | _todo_ | _todo_ | — |
| v11 generated protos decode v10 replies | — | — | _todo_ |
| Exec plugin lifetime, schematic scope | — | _todo_ | — |
