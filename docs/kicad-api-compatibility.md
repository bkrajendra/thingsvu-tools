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
| `api-server --socket <path>` | ✅ honored: pipe `\\.\pipe\<path>` (verified with `…\run\k.sock`). One earlier attempt with `…\run\api.sock` never became reachable; cause not yet known. With stdout redirected the server prints nothing (no "listening" line), so readiness must be detected by polling | — | — |
| `api-server` with a hand-written minimal `.kicad_sch` | `Failed to load schematic` — use KiCad-authored fixtures | — | — |
| Default endpoint (no `--socket`) | `ipc://C:\Users\<user>\AppData\Local\Temp\kicad\api.sock` → named pipe `\\.\pipe\C:\Users\<user>\AppData\Local\Temp\kicad\api.sock` | ✅ same default endpoint | ✅ same default endpoint (when it is the only KiCad running) |
| Two GUI instances at once (nightly started 15:17:13, 10.0.7 at 15:17:29, both with the API server enabled) | — | The first instance (nightly) owns `api.sock`; 10 of 10 connections reached it | **No reachable endpoint** for the second instance: not `api.sock`, not `api-<pid>.sock`. Multi-instance discovery cannot rely on per-instance socket names on Windows |
| Endpoint visible in `\\.\pipe\` listing | Not listed by `Directory.GetFiles('\\.\pipe\')` — the name contains backslashes. Discovery must derive the name from the socket path, not enumerate pipes | same | same |
| NNG handshake (SP header, REQ0 ↔ REP0) | ✅ peer replies `00 53 50 00 00 31 00 00` (REP0) | ✅ | ✅ |
| NNG ipc framing | ✅ `0x01` + u64 big-endian length + body; REQ body prefixed with 4-byte request id (high bit set), echoed in reply | ✅ | ✅ |
| `GetVersion` | ✅ `major=10 minor=99 patch=0`, `full_version="10.99.0-5896-gd033635628"` | ✅ same | ✅ `major=10 minor=0`, `full_version="10.0.7"`, decoded with the KiCad 11 protos |
| `Ping` | ✅ reply `google.protobuf.Empty` | ✅ | _not run_ |
| Token: empty token | Accepted; reply header carries the instance token (UUID form, stable across calls and connections) | Accepted | Accepted; reply header carries the instance token |
| Token: mismatch | _not tested_ | ✅ wrong pinned token → `AS_TOKEN_MISMATCH` "the provided kicad_token did not match this KiCad instance's token" | _not run_ |
| Instance token per instance | Differs per server instance (`2757…` → `2e39…` after restart): usable to pin a session | — | — |
| `GetOpenDocuments` (SCHEMATIC / PCB / PROJECT) | Without a loaded file: `AS_UNHANDLED` for all types. With `api-server <file>.kicad_sch`: SCHEMATIC ✅ `DocumentSpecifier{type=SCHEMATIC, project{name, path}}` (no sheet path); PCB `AS_UNHANDLED` | SCHEMATIC ✅ (schematic editor open) | SCHEMATIC ✅ but identifies the document **only by filename** (field 4, e.g. `kicad-demo10.1.kicad_sch`), with **no project block**; PCB `AS_UNHANDLED` |
| `GetSchematicHierarchy` | ✅ root `SheetInstance{path=[root KIID], "/", name, filename, page "1"}`. Root KIID equals the file's top-level `(uuid …)` — file UUIDs and IPC KIIDs are the same identity | _not run_ | `AS_UNHANDLED` |
| `GetSchematicNetlist` | ✅ nets list **pin KIIDs** per sheet (map to REF.PIN via each instance's pins); unconnected pins appear as `unconnected-(R1-Pad1)` | ✅ | `AS_UNHANDLED` |
| `LoadAllLibraries{LT_SYMBOL}` | ✅ `LCS_OK`, async; 223 symbol libraries. `GetLibraryStatuses` reported `Device` as status 1 (`LLS_INVALID`) yet placement succeeded 1 s later — re-check | ❌ `AS_UNIMPLEMENTED` "LoadAllLibraries is not available in GUI mode" (the proto comment says no-op; send it only in headless sessions) | `AS_UNHANDLED` |
| `PlaceSymbolFromLibrary` before library load | `AS_BAD_REQUEST` "symbol 'Device:R' not found" | — (GUI preloads; placement immediate) | — |
| `PlaceSymbolFromLibrary` | ✅ full `SchematicSymbolInstance`: new KIID, sheet path, position (nm), library symbol with per-instance pin KIIDs (pins 1/2 at local y = ∓3.81 mm), fields (Reference as requested, Value = "R") | ✅ | `AS_UNHANDLED` |
| `UpdateItems` (instance with edited `value_field.text`) | ✅ `ISC_OK`; Value = "10k". Full-object replace, no field mask | ✅ | _not run_ (no schematic item commands) |
| `CreateItems` (SchematicLine wire, 3× LocalLabel) | ✅ `ISC_OK` per item, new KIIDs, default stroke/text attributes | ✅ | _not run_ (KiCad 10 has no `SchematicLine` type) |
| `BeginCommit` / `EndCommit` | **Depends on how the document was opened.** Server started *with* a file argument: `BeginCommit` → `AS_UNHANDLED`. Document opened with `OpenDocument`/`CreateDocument` (what sessions do): commits ✅. **Inside an open commit a newly placed symbol is not yet in the document**: `UpdateItems` on it fails "an item with UUID … does not exist". Place in one commit, then update in a second | ✅ commit opened and closed around place + update + create. **Commits are owned by `client_name`**: another client gets `AS_BAD_REQUEST` "the client … does not has a commit in progress". A commit left open by a crashed client did not block a new client's commit. **One commit = one undo step**: a single Ctrl+Z removed the whole demo (user-verified) | `BeginCommit`/`EndCommit(CMA_DROP)` ✅ answered, but no schematic item commands exist to use inside a commit |
| Connectivity read-back after edits | ✅ `/INPUT` = {R1.1}, `/MID` = {R1.2, R2.1}, `/OUTPUT` = {R2.2}; labels exactly on pin endpoints connect | ✅ same | — |
| `SaveCopyOfDocument` (`include_project=true`) | ✅ writes `.kicad_sch`, `.kicad_pro`, `.kicad_prl`; loaded document unchanged. Target directory must exist | ✅; open document and file on disk unchanged | `AS_UNHANDLED` |
| `RunSchematicJobExportNetlist` (KiCad s-expr) | ❌ **reads the saved file, not the in-memory document**: on a never-saved document the job fails "Failed to load schematic" (exit 3) | ✅ `JS_SUCCESS`, netlist identical to the read-back (the document had been saved) | — |
| `RunSchematicJobExportSvg` | ❌ same as netlist job (PNG job tested): reads the saved file | ✅ `JS_SUCCESS`, output dir → `<project>.svg` | — |
| ERC over IPC | No ERC job exists in `schematic_jobs.proto` (exports only: SVG, DXF, PDF, PS, PNG, netlist, BOM). ERC stays on `kicad-cli` | same | — |
| `SaveDocument` (in `project_commands.proto` in KiCad 11) | ✅ writes the document to its path; modified state → `DMS_UNMODIFIED` | _not run_ | _not run_ |
| `RevertDocument` | ✅ discards unsaved edits (R2 gone, R1 kept); state → `DMS_UNMODIFIED` | _not run_ | _not run_ |
| Saved copy verified by `kicad-cli` 10.99 | ✅ netlist matches intent; ERC 2 warnings (`isolated_pin_label`, expected) | — | — |
| KiCad 11 file read by 10.0.7 `kicad-cli` | ❌ "Failed to load schematic" (exit 3), as expected from format `20261015` | — | — |
| Native Rust transport + client (`crates/kicad-ipc`, `examples/probe.rs`) | ✅ full demo from a fresh empty fixture | ✅ full `gui` scenario | ✅ `basic` |
| Unknown command type | `AS_UNHANDLED` "no handler available for request of type …" | — | `AS_UNHANDLED`, same message format |
| Robustness | Repeated connect/handshake/close and bare pipe connect/close left the server responsive | Interrupted client with an open commit did not affect later clients | — |
| v11 generated protos decode v10 replies | — | — | ✅ envelope, status, `GetVersion`, `GetOpenDocuments`, commit replies |
| Exec plugin lifetime, schematic scope | — | _deferred to Phase 4_ (the headless path does not depend on it) | — |
| Headless document lifecycle (server started without a file) | ✅ `CreateDocument` (schematic, in memory; no file until saved) → `GetDocumentModifiedState` `DMS_UNMODIFIED` → edit → `DMS_MODIFIED` → `SaveDocument` → `RevertDocument` → `CloseDocument` (document commands then `AS_UNHANDLED` again) | — | — |
| Generic editor commands in the schematic editor | — | — | `GetItems`, `GetSelection`, `SaveCopyOfDocument` → `AS_UNHANDLED`. Live KiCad 10 schematic access is limited to version, open-document filename, and commits |
| `GetItemsFromLibrary` (symbols) | ✅ returns `SchematicSymbol` per id with reference prefix, `unit_count`, and pins (number, name, local position in the placed y-down axes, unit). Unknown entries are omitted | _not run_ | — |
| `UpdateItems` of one field with `ItemHeader.field_mask` and no embedded `definition` | ✅ value changes, no duplicate library symbol (one or two instances) | ⚠ the earlier full-object update inside a commit replied `10k` but did not persist; the masked form is untested in the GUI | — |
| `DeleteItems` (cleanup of placed symbols) | ✅ used to undo a failed apply; document returned to its starting fingerprint | _not run_ | — |

## File provider observations (KiCad 10.0.7, Phase 6)

Observed 25 Oct 2026 while building the file provider (`kicad-sexpr`, `FileSession`).

| Topic | Observation |
|---|---|
| Schematic format | 10.0.7 writes `(version 20260306)`, `(generator_version "10.0")`. The 10.99 nightly writes `20261015`. The 10.0 CLI refuses `20261015` files ("Failed to load schematic"); the 10.99 CLI reads both. |
| Symbol library format | 10.0.7 libraries are `(version 20251024)`, `(generator_version "10.0")`; same structure as the nightly's for everything the layout reads. |
| `kicad-cli sch upgrade --force <file>` | 10.0.7 rewrites an older schematic (e.g. a bundled demo, format `20250114`) in the current format. Used to make KiCad-authored fixtures. |
| Instance syntax | Identical in 20260306 and 20261015: `lib_id`, `at`, `(mirror x\|y)`, `unit`, `body_style`, flags, `uuid`, `property`×5, `pin`, `instances`. Wires use `(stroke (width 0) (type default))`; labels use `(justify left\|right bottom)` with angle 0/90/180/270; junctions `(diameter 0) (color 0 0 0 0)`. |
| Field text | A property's `(at x y angle)` is the library's offset moved by the symbol's transform; the **angle is stored relative to the symbol** (a fuse rotated 90° keeps the library's 90 for its reference and is drawn horizontal). |
| Pin geometry | Library y-up coordinates map to the API's y-down ones by negating y; the library's pin angle points from the connection point toward the body. The geometry read from 10.0.7's library files equals the 10.99 API's for every symbol of the golden circuits. |
| `extends` | A derived symbol (`NE555P` extends `NE555D`) is embedded in schematics as one flattened symbol; its sub-symbols take the derived name. |
| Netlist | `kicad-cli sch export netlist` omits power symbols and flags (`#PWR…`, `#FLG…`), unlike the API's `GetSchematicNetlist`. Net names are still set by power symbols (`+5V`, `GND`). |
| Lock file | An editor with a schematic open leaves `~<name>.kicad_sch.lck` beside it (seen beside the demo project while the nightly had it open). |
| PNG export | `kicad-cli sch export png` exists only in the 10.99 CLI; 10.0.7 has `svg`, `pdf`, `ps`, `dxf`, `hpgl`. |
| 10.99 CLI on 10.0 files | Reads a 20260306 file and exports PNG of it; used for rendering in file sessions. |
| Library lookup | Global libraries live in `<install>/share/kicad/symbols/<nickname>.kicad_sym` (10.0 and 10.99 on Windows). The project's own `sym-lib-table` can name others; the user's global table (custom libraries) is not read. |
| KiCad 10 GUI loading of a generated file | **Not yet checked by the author**: loaded by `kicad-cli` 10.0.7 (netlist, ERC) only. See docs/tasks.md, Phase 6 notes. |

## Re-pinning to a new KiCad build

Do this when KiCad 11.0 is released (the nightly `10.99` stops being the target), and whenever the pinned nightly is replaced.

1. Install the new build. Update the *Pinned builds* table above.
2. Run `scripts\probe-kicad.ps1` against the new build and compare with the probe table. Any difference is a finding to record here.
3. If the API protos changed, vendor them again from the new KiCad commit into `crates/kicad-ipc/proto/`, regenerate `SHA256SUMS`, and update the commit in this file. Rebuild; fix what the new definitions break.
4. Run the whole live suite: `$env:KICAD_LIVE = '1'; cargo test --workspace -- --test-threads=1`. The `library_geometry_matches_kicads_own` and `both_backends_build_the_same_circuit` tests in `kicad-session` are the ones that notice a format or geometry change.
5. If the schematic format version changed, extend the supported range (`MIN_VERSION` / `MAX_VERSION` in `crates/kicad-sexpr/src/sch.rs`) only after the live tests pass on files of the new format, and add its series to `format_of` in `crates/kicad-session/src/file.rs`.
6. Update `docs/capability-matrix.md`, the README's supported-version table, and the badges.

## Reading and editing a whole sheet over the API (10.99, Phase 8)

Observed 25 Oct 2026.

| Topic | Observation |
|---|---|
| `GetItems` with many types | One request with the types for lines, junctions, local/global/hierarchical/directive labels, no-connect markers, bus entries, sheets, text, text boxes, shapes, images, tables, groups, and rule areas answers in one reply on the headless server. A `SchematicLine` has a type: wire, bus, or graphic |
| `DeleteItems` | Deletes wires, labels, junctions, and placed symbols (including power symbols) by KIID; each item has its own status (`IDS_OK`, `IDS_NONEXISTENT`, …) |
| `UpdateItems` with `field_mask` `position` and `transform` | Moves and turns a symbol. **The symbol's reference, value, and other field text stay where they were**: they are not carried along. Fields must be updated separately (`reference_field`, `value_field`, …, each carrying its own `text.position`) |
| Field text angle | `text.attributes.angle` is the same value the file stores: relative to the symbol. It does not change when the symbol is turned, and leaving it alone gives the same drawing as KiCad's own move in the editor |
| Wires after `CreateItems` | KiCad joins wires that meet end to end into one, so a set of wires read back may be cut differently from the set that was created; compare wires as runs along a line |
| `kicad-cli` netlist vs API netlist | The CLI netlister omits power symbols and flags; the API netlist includes them. A flag that is connected to the rest of a net only through the name of a power symbol on its wire has to be attached by name |
| ERC `power_pin_not_driven` | One violation per net. A net named by a **power symbol** whose pins nothing drives (no `PWR_FLAG`, no output pin) gets it; the same net named by a **label** does not. A tidy must therefore not turn a label-named net into a power-symbol net |
