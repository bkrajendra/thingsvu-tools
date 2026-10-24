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

- [ ] **3.1 `OperationPlan`** in `kicad-domain` — ordered operations with preconditions, document identity, and limits (plan §7.2, §10).
- [ ] **3.2 Layout for `CircuitSpec`** — deterministic grid placement; wires and labels on computed pin points.
- [ ] **3.3 `sch_plan_changes`** — validate and return a plan id; nothing is applied.
- [ ] **3.4 `sch_apply_plan`** — `confirm: true` plus idempotency key. One commit in GUI sessions; a recorded sequence with cleanup on failure headless. Read back after applying.
- [ ] **3.5 Field updates** — `UpdateItems` without `definition`, with a field mask; verify in GUI and headless that no duplicate library symbol appears and the value persists (follow-up from 2.6).
- [ ] **3.6 Transforms** — set rotation at placement; check any transform written by `UpdateItems` by reading it back (follow-up from 2.5).
- [ ] **3.7 `sch_save`, `sch_run_erc`, `sch_export_netlist`, `sch_render`.**
- [ ] **3.8 `sch_create_demo_circuit`** — the two-resistor fixture as plan → apply → verify through MCP.
