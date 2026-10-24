# Tasks

Phase-by-phase task list for the plan in [kicad-ai-mcp-design-plan.md](kicad-ai-mcp-design-plan.md) (§14).

Legend: `[x]` done · `[ ]` open · `[~]` partly done or deferred.
Notes marked **Review** record a decision or workaround that needs the project owner's review.

---

## Phase 0 — Compatibility spike

- [x] **0.1 Validate the development environment** — nightly `10.99.0-5896-gd033635628` and stable `10.0.7` installed; `KICAD_HOME`, `kicad-cli`, `protoc`, and `cmake` on `Path`.
- [x] **0.2 Choose the license** — GPL-3.0-or-later.
  - Vendored protos: 20 GPL-3.0-or-later and 4 MIT, both compatible.
- [x] **0.3 Vendor the KiCad API protos** — commit `d033635628`, 24 files, `crates/kicad-ipc/proto/SHA256SUMS`.
- [x] **0.4 Probe the wire protocol** — NNG REQ0 over a Windows named pipe; handshake and framing verified (`scripts/probe-kicad.ps1`).
- [x] **0.5 Native Rust transport and client** — `crates/kicad-ipc`; no C NNG library.
  - **Review:** the NNG subset is implemented natively instead of using the `nng` crate, so the build needs neither CMake nor libnng. It sits behind the transport module, so the C library can be swapped back in if needed.
- [x] **0.6 Headless schematic probe (nightly)** — place, values, wire, labels, netlist read-back, save copy, `CreateDocument` / save / revert / close.
- [x] **0.7 GUI probe (nightly)** — the demo inside one commit is a single undo step (user-verified); token mismatch refused; IPC netlist and SVG jobs work.
- [x] **0.8 GUI probe (KiCad 10.0.7)** — schematic commands unhandled; open documents reported by filename only; v11 protos decode its replies.
- [x] **0.9 Fixtures** — `tests/fixtures/v11/` (empty schematic, two-resistor chain).
- [x] **0.10 Compatibility record and plan updates** — `docs/kicad-api-compatibility.md`, plan §3.1, §3.3, §5.2, §14.
- [~] **0.11 Executable plugin lifetime (schematic scope)** — deferred to Phase 4; the headless path does not depend on it.

Phase 0 notes for review:
- **Review — toolchain:** `rust-toolchain.toml` uses `stable` (installed 1.99.1) instead of an exact pin, to avoid a duplicate toolchain download. The minimum version is enforced by `rust-version = "1.99"`.
- **Review — open questions to revisit:**
  - One `api-server --socket …\api.sock` start never became reachable, while `k.sock` / `k2.sock` / `k3.sock` worked. Cause unknown.
  - `GetLibraryStatuses` reported `Device` as `LLS_INVALID` while placement worked.
- **Review — multi-instance:** on Windows only the first KiCad GUI gets an API endpoint, so two GUIs cannot be driven at once.

---

## Phase 1 — Foundations

Exit criteria (plan §14): unit and protocol tests green on Windows CI; `session_status` from a real MCP host against the nightly headless server.

- [x] **1.1 `kicad-domain` crate** — validated ids (`LogicalId`, `Kiid`, `LibraryId`); integer-nanometre geometry with 1.27 mm grid checks/snapping and 90° orientations; `CircuitSpec` (loads the example JSON, rejects unknown fields); validation (duplicates, unknown components, pins in two nets, limits); `CapabilitySet`; result envelope (`ResultState`, `Provenance`, `ErrorCode`, `ToolError`). 16 unit tests.
  - **Review:** rotation/mirror conventions (CCW on screen, rotate then mirror) are only verified for the unrotated case. Confirm against KiCad read-back in Phase 2 before the layout engine relies on them.
  - **Review:** single-pin nets are a warning (`SINGLE_PIN_NET`), matching KiCad's ERC `isolated_pin_label` warnings on the fixture.
  - Decision: `OperationPlan` moves to Phase 3, where it is first used.
- [x] **1.2 Capability probing in `kicad-ipc`** — `capabilities::probe` reports per-command support using side-effect-free requests: reads, an empty `CreateItems` batch, placement of a nonexistent symbol, and a commit opened and immediately dropped. Verified on the live KiCad 10.0.7 GUI.
  - **Review:** export jobs are not probed, because any accepted request would write a file. Their support is learned on first use.
  - **Review:** an empty `CreateItems` batch is accepted by KiCad 10's generic handler although it has no schematic item types. "Can edit schematics" must require both `PlaceSymbolFromLibrary` and `CreateItems`, never `CreateItems` alone.
- [x] **1.3 `kicad-ipc-mock` crate** — fake KiCad server on a real named pipe (Unix socket elsewhere); per-message handlers; token checks; request log. 6 integration tests covering token pinning, unhandled and mismatch mapping, sequential connections, and probes against simulated KiCad 10 GUI and KiCad 11 headless.
- [x] **1.4 `kicad-cli-runner` crate** — install discovery (configured → `KICAD_HOME` → `PATH` → standard locations, newest version first); version parsing (`10.99` counts as series 11); `run_erc` (JSON report parsed); `export_netlist` (s-expression netlist parsed, topology comparison); no shell, no console window, kill on timeout. 5 unit tests plus 2 live tests (`KICAD_LIVE=1`).
  - **Review:** a file newer than the CLI is detected from kicad-cli's "Failed to load" message (`CliError::LoadFailed`), not by comparing format versions up front.
  - **Attention (Phase 3):** headless files whose values were set by sending the whole symbol back (`UpdateItems` including `definition`) gain a duplicate library symbol `R_1`, and the netlist then reports `libsource` `:R_1`. GUI-saved files do not. Phase 3 should update only the changed field (`ItemHeader.field_mask`) and verify no duplicate is created. The `two-resistor-series-chain` fixture contains this duplicate.
- [x] **1.5 Headless session provider** — `crates/kicad-session`:
  - Starts `kicad-cli api-server` with no file on a per-session socket and polls connect + `Ping` for readiness.
  - Opens with `OpenDocument`, or creates with `CreateDocument`; then `LoadAllLibraries`; then probes capabilities.
  - One lock serializes all requests; each session has a stable client name.
  - Refuses a file with a KiCad lock file (`~<name>.lck`); refuses to close with unsaved changes unless `discard_unsaved`.
  - 6 unit tests plus 4 live tests, including a crashed owner whose KiCad server is killed by the OS.
  - **Review — orphan protection:** on Windows the server is placed in a Job Object with kill-on-close, so it dies even if `kicad-ai-mcp` crashes (verified). On Linux/macOS only `kill_on_drop` applies, which a crash bypasses. Add a parent-death signal when those platforms are targeted.
  - **Review — crate layout:** the plan put the headless code inside `kicad-ai-mcp`; it lives in its own library crate, `kicad-session`, so it can be tested and reused by the Phase 4 plugin bridge. Plan §4 updated.
  - **Review — socket location:** sockets are named `<temp>\thingsvu\<session-id>.sock`. On Windows that is a named pipe created by KiCad with its default security, which grants write access only to the owner, administrators, and SYSTEM. A custom DACL would need KiCad support.
  - **Review — instance token:** `session_status` never shows the KiCad instance token, only an 8-hex fingerprint of it.
  - **Review — saving inferred:** `sch.save` is inferred from `GetDocumentModifiedState`, since probing `SaveDocument` would write the file.
- [x] **1.6 `kicad-ai-mcp` skeleton** — rmcp 3.7 stdio server (`crates/kicad-ai-mcp`):
  - Logs go to stderr only.
  - Tools: `session_status`, `session_open`, `session_close`.
  - Every result uses the plan §7.4 envelope as MCP structured content (`ok`, `result_state`, `result`, `provenance`, `warnings`, `correlation_id`), and errors also set `isError`.
  - Inputs reject unknown fields.
  - `--kicad-cli <path>` prefers a specific CLI. Release binary is 4.5 MB.
  - **Review:** the protocol test negotiates MCP `2025-11-25`, the newest version that still has `initialize`; rmcp also supports `2026-07-28`.
- [x] **1.7 End-to-end check** — passed with a scripted MCP client over stdio driving the real binary: handshake, tool list, error envelopes, only protocol messages on stdout, and a live headless open/status/close on the nightly. Client setup is in `examples/README.md`.
  - Confirmed by the project owner with a real MCP host: `kicad-ai` added to Claude, `session_open` and `session_close` worked (24 Oct 2026).
- [x] **1.8 CI** — `.github/workflows/ci.yml`: Windows job (fmt, clippy `-D warnings`, tests) and a Linux job (build and tests, `continue-on-error`). First hosted run (`0ead3d2`): both jobs passed, Linux included.
  - **Review:** the Linux job is informational only. The Unix code paths (Unix sockets, no job object) compile in principle but have not been run.

---

## Phase 2 — KiCad 11 read path

Exit criteria (plan §14): read tools on IPC whose results agree with KiCad-authored fixtures.

- [x] **2.1 `sch_list_symbols`** — `GetItems(KOT_SCH_SYMBOL)` mapped to id, reference, value, library id, footprint, position (mm), orientation, unit, sheet path; sorted naturally (R2 before R10); provenance `kicad_ipc`.
  - **Review:** reads the document's root sheet only (the `DocumentSpecifier` from `OpenDocument` has no sheet path). Hierarchical sheets are out of MVP scope; revisit when multi-sheet support starts.
- [x] **2.2 `sch_get_symbol_pins`** — pins of the symbol's unit (unit 0 = shared), with number, name, electrical type, connection point on the sheet, and the symbol-local point. Selector is a KIID, or a reference matching exactly one symbol; otherwise `SYMBOL_NOT_FOUND`, or `INVALID_ARGUMENT` listing the candidate ids.
  - **Review:** body styles (De Morgan alternates) are not filtered yet; every pin of the unit is returned.
- [x] **2.3 `sch_get_netlist`** — `GetSchematicNetlist`, with pin KIIDs resolved through every instance's pins to (reference, pin, symbol id). Wires, labels, and other items are counted per net. The result includes an `unconnected_pins` count (nets KiCad names `unconnected-…`).
- [x] **2.4 `session_list_documents`** — schematic and PCB documents from `GetOpenDocuments`; editor types with nothing open are skipped.
- [x] **2.5 Verify orientation transforms** — live test with `Device:Q_NPN` (pins B/C/E, asymmetric) in 4 rotations × {none, mirror_x, mirror_y}. A label placed where our transform puts each pin touches exactly that pin in KiCad's netlist, for all 36 pins. `Orientation::apply` is correct for KiCad's read-back transforms.
  - **Attention (Phase 3):** KiCad stores transforms in its own normalized form, and setting one by `UpdateItems` is not straightforward: requesting 180° + `mirror_x` read back as identity, and 90° + `mirror_y` read back as 270° + `mirror_x`. Phase 3 must check every transform it writes by reading it back, and should place symbols with the rotation set at placement time.
- [x] **2.6 Fixture refresh** — `two-resistor-series-chain` regenerated headless: one library symbol `Device:R`, R1/R2 = `10k`, netlist parts `R`, ERC unchanged (2 expected warnings). CLI test data regenerated from it, and the netlist test now fails if a duplicate library symbol appears.
  - **Finding (feeds Phase 3):** how `UpdateItems` behaves when setting a value:
    - Sending the whole symbol (embedded library definition included) for two instances in one headless batch created a duplicate library symbol `R_1`.
    - The same request in a GUI commit replied `10k` but the saved file kept `R`.
    - Sending the instance without `definition` and with field mask `value_field` works headless for one and for two symbols. The probe now does this.
    - **Attention:** the GUI path for value updates is not yet verified with the new form; Phase 3 must test it.

Phase 2 exit check: met. The read tools agree with the KiCad-authored fixture, both through `kicad-session` (`tests/live_read.rs`) and through the MCP binary (`kicad-ai-mcp/tests/stdio.rs`, `live_read_tools_on_the_fixture`).

---

## Phase 3 — KiCad 11 mutation path (headless first)

Exit criteria (plan §14): all Tier 1 headless acceptance criteria (plan §9). The two-resistor demo goes end to end through MCP tools.

- [x] **3.1 `OperationPlan`** (`kicad-domain/src/plan.rs`) — operations `place_symbol`, `add_wire`, `add_label`; expected nets as `REF.PIN` sets; references per component; findings. Plans are deterministic. Limit of 200 operations.
  - **Review:** plan §10 suggested at most 50 operations per plan. The label-per-pin layout needs more, so the limit is 200.
  - **Review — staleness:** KiCad's IPC exposes no document revision, so a plan records a fingerprint of the document (symbols with ids, references, values, positions, orientations, plus net topology). Any change to these makes the plan `STALE_PLAN`. Changes that touch neither (e.g. moving a label without changing connectivity, editing text) are not detected.
- [x] **3.2 Layout** — symbols on a grid (5 per row, cell size from the largest symbol's pin extent plus margin), below any existing symbols. A two-pin net on one horizontal or vertical line with no pin in between becomes a wire with one label; any other net gets a same-name local label on each pin. References: the preferred one if free, otherwise the library prefix plus the next free number. Pin geometry comes from KiCad's library before placement (`GetItemsFromLibrary`). Multi-unit symbols are refused (`MULTI_UNIT_UNSUPPORTED`).
  - **Review — readability:** labels sit directly on pin ends and crowd the reference/value text (see a `sch_render` of the demo). Connectivity is correct. A later layout pass should add short wire stubs leading away from each pin, with the label at the stub end (needs the pin direction from the library).
- [x] **3.3 `sch_plan_changes`** — takes a `CircuitSpec` JSON; nothing is changed. Returns `plan_id`, `can_apply`, counts, references, operations in mm, expected nets, and findings.
- [x] **3.4 `sch_apply_plan`** — needs `confirm: true` and an `idempotency_key` (8–128 characters). Repeating a key replays the earlier result; reusing an applied plan, or a plan whose fingerprint no longer matches, gives `STALE_PLAN`. Applies in **two commits** (placement, then values/wires/labels): inside an open commit a newly placed symbol is not yet in the document, so it cannot be updated (finding below). A failed step's commit is dropped and earlier steps' items are deleted, giving `rejected` with `mutation_state: not_applied` (tested). Afterwards connectivity is read back and compared pin-for-pin with the expected nets; a mismatch gives `result_state: partial`.
  - **Finding:** headless sessions *do* support commits when the document is opened with `OpenDocument`/`CreateDocument`. Phase 0 tested a server started with a file argument, which does not. Compatibility record corrected.
  - **Review — undo granularity:** one apply is **two** undo steps in the GUI, not one as the plan intended. Merging them needs either a placement call that sets field values, or KiCad exposing staged items to `UpdateItems`.
  - **Review — key reuse after failure:** a failed apply that changed nothing (refused before any request) can be retried with the same key. A failed apply after KiCad was contacted marks the plan and key as used, so the caller must make a new plan.
- [x] **3.5 Field updates** — `UpdateItems` without the embedded `definition`, with a field mask (`value_field`, plus `footprint_field` when set). Each value is read back after updating; headless, no duplicate library symbol appears.
  - Verified in the GUI on 24 Oct 2026 (task 4.7): values persist with the masked update.
- [~] **3.6 Transforms** — placement sets rotation through `PlaceSymbolFromLibrary` (0° is sent as "unset"), and position and reference are read back after placing. The layout uses rotation 0 only, so writing other rotations and mirrors is not exercised yet. Follow-up when the layout starts rotating symbols: read back every written transform (see 2.5).
- [x] **3.7 Save, ERC, export, render** — `sch_save` (`SaveDocument`); `sch_run_erc`, `sch_export_netlist` (to a `.net` inside the schematic's folder), and `sch_render` (PNG returned as an MCP image, 72–600 dpi) all run `kicad-cli` on a `SaveCopyOfDocument` snapshot in a private folder, which is deleted afterwards.
  - **Finding:** KiCad's IPC export jobs read the saved file, not the in-memory document. A never-saved document fails "Failed to load schematic", and a saved one would silently ignore unsaved changes. Hence the snapshot approach.
  - **Review:** `sch_render` returns the first page only; multi-sheet designs are out of MVP scope.
  - **Review — read-only mode:** `--read-only` refuses `sch_apply_plan`, `sch_save`, `sch_export_netlist`, and the apply step of `sch_create_demo_circuit`. Planning, reads, ERC, and rendering stay allowed; the latter two only write to private scratch folders.
- [x] **3.8 `sch_create_demo_circuit`** — plans the bundled two-resistor spec, and applies it with `apply`, `confirm`, and `idempotency_key`.

Tests: `kicad-domain` 20 unit tests (layout, references, unknown pins, wire rule); `kicad-session/tests/live_mutate.rs` (plan → apply → verify → save → kicad-cli; stale plan; cleanup after a failed placement); `kicad-ai-mcp/tests/stdio.rs` (whole demo through the MCP binary, including idempotent replay, stale plan, ERC, render image, export path rule, save, independent kicad-cli check; read-only mode). All pass offline and live.

Phase 3 exit check: met for headless. The demo is planned, applied, verified, saved, and checked independently through MCP tools on the nightly. The GUI counterpart is Phase 4.

---

## Phase 4 — KiCad 11 GUI provider (plugin)

Exit criteria (plan §14): all Tier 1 acceptance criteria. The demo works live in the Schematic Editor, with undo.

- [x] **4.1 `kicad-ai-plugin`** (`crates/kicad-ai-plugin`) — executable plugin with `register` (default), `unregister`, `list`. `register` reads `KICAD_API_SOCKET` and `KICAD_API_TOKEN`, confirms the instance answers with that token pinned, records it, prints one status line (shown in KiCad's message area), and exits.
  - **Review — registrar instead of bridge:** the plan described a long-running bridge process that relays requests over its own local protocol (`kicad-ai-bridge/v1`). KiCad's API accepts any local client, and the token only pins the instance. So the plugin is a registrar: the MCP server connects to the registered KiCad itself, with the token pinned. No relay protocol, no plugin process to keep alive or orphan; the plugin-lifetime question from 0.11 no longer applies. Plan §1, §3.1, and §6 are updated to match.
  - **Review — token storage:** the instance token is written to `%LOCALAPPDATA%\thingsvu\kicad-ai\instances\<hash>.json` (user-only folder by default). It is never shown in tool results or logs; `session_status` lists only version and projects. It is an instance identifier, not an access secret (KiCad accepts tokenless local clients).
- [x] **4.2 Registry** (`kicad-session::registry`) — one JSON record per KiCad endpoint (FNV hash of the socket path), atomic writes, newest first. Unreachable or token-mismatched records are removed when `session_status` or `session_open` looks at them. `THINGSVU_REGISTRY_DIR` overrides the folder for tests.
- [x] **4.3 GUI session provider** — `Session::open_gui`: connects with the token pinned, picks the root schematic by `schematic_path` or the only open one, probes capabilities in GUI mode, and pairs the session with the `kicad-cli` of the same KiCad series. Closing only disconnects: the editor, document, and unsaved changes stay with the user. Provenance reports `provider: gui`.
- [x] **4.4 MCP** — `session_open` takes `provider: "gui"` (schematic path optional); `session_status` lists `gui_editors` (version and projects).
- [x] **4.5 Packaging** — `plugin-package/plugin.json` (schema-valid for the 10.99 build: `exec` runtime, `schematic` and `pcb` scopes), toolbar icons (light/dark, 24/48 px), `scripts/install-plugin.ps1` (resolves the real Documents folder, e.g. OneDrive; refuses KiCad < 10.99), and `scripts/uninstall-plugin.ps1`. Installed into the nightly's plugin folder on the development machine.
- [x] **4.6 Automated test of the plugin path** — `kicad-ai-plugin/tests/live_plugin.rs`: a headless KiCad stands in for the editor (it has a socket and a token, as KiCad gives a plugin). Covers the real plugin binary registering and unregistering; the GUI provider opening by path and by default; refusal of an unopened path and of a wrong token; closing without stopping KiCad; and the explanatory message when run outside KiCad.
- [x] **4.7 Live GUI check** — run on 24 Oct 2026 against the nightly Schematic Editor (`gui_editor_demo`, `THINGSVU_GUI=1`), project `D:projectskicad-demo` (saving approved by the project owner):
  - The toolbar button registered the editor; `session_status` listed it; `session_open` with `provider: gui` found `kicad-demo.kicad_sch` on its own.
  - The demo applied live and was verified pin for pin; `undo_steps: 2`.
  - **Values persist in the GUI** with the masked update (R1 and R2 are `10k`). This closes the attention item from 3.5.
  - ERC on a snapshot: 0 errors, 2 expected warnings. `sch_render` returned a PNG.
  - **`sch_save` works in the GUI** (`SaveDocument`). The saved file, checked with `kicad-cli`: nets `/INPUT`=R1.1, `/MID`=R1.2+R2.1, `/OUTPUT`=R2.2; values `10k`; one library symbol.
  - Undo confirmed by the project owner: two Ctrl+Z steps remove the demo (values and connections, then placement).

Phase 4 exit check: met. The demo works live in the Schematic Editor with undo, and through the headless provider.
  - **Finding (fixed, 24 Oct 2026):** the first install did not show the button. KiCad (`common/api/api_plugin.cpp`) **skips any action whose `entrypoint` is an absolute path**, and only logs it in API trace mode. The manifest now uses the plugin-relative `kicad-ai-plugin.exe`, and the template is shipped as `plugin-package/plugin.json`.
  - **Finding — plugin output:** for an async action, KiCad shows only a failing action (non-zero exit, with stderr as detail); stdout is never shown, and stderr after a success is shown as an error. Failures therefore go to stderr with exit code 1. Success shows a small Windows message box ("KiCad AI — Connected …"), chosen by the project owner on 24 Oct 2026. It blocks only the plugin process, and `THINGSVU_NO_DIALOG` turns it off for tests.
  - Where to find it: Schematic Editor top toolbar, after "Switch to PCB Editor" and the variant selector; also **Tools → External Plugins** (which has **Refresh Plugins**).
