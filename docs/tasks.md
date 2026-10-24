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
- [~] **1.7 End-to-end check** — passed with a scripted MCP client over stdio driving the real binary: handshake, tool list, error envelopes, only protocol messages on stdout, and a live headless open/status/close on the nightly. Client setup is in `examples/README.md`.
  - **Attention:** check with a real MCP host (Claude Code or Claude Desktop), which cannot be run from inside the development session. Steps are in `examples/README.md`.
- [~] **1.8 CI** — `.github/workflows/ci.yml`: Windows job (fmt, clippy `-D warnings`, tests) and a Linux job (build and tests, `continue-on-error`). The same commands pass locally. The first hosted run happens on the next push.
  - **Review:** the Linux job is informational only. The Unix code paths (Unix sockets, no job object) compile in principle but have not been run.

---

## Phase 2 — KiCad 11 read path

Exit criteria (plan §14): read tools on IPC whose results agree with KiCad-authored fixtures.

- [ ] **2.1 `sch_list_symbols`** — UUID, reference, value, library id, position (mm), sheet path, with provenance.
- [ ] **2.2 `sch_get_symbol_pins`** — pin number/name/type and world position for one symbol (UUID, or an unambiguous reference).
- [ ] **2.3 `sch_get_netlist`** — nets as (symbol, reference, pin) tuples, mapped from KiCad's pin KIIDs.
- [ ] **2.4 `session_list_documents`** — open documents per session.
- [ ] **2.5 Verify orientation transforms** — compare `Orientation::apply` with KiCad read-back for rotated and mirrored symbols (follow-up from 1.1).
- [ ] **2.6 Fixture refresh** — regenerate `two-resistor-series-chain` without the duplicate `R_1` library symbol, once field-only updates exist (follow-up from 1.4; may move to Phase 3).
