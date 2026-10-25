# KiCad AI MCP + Rust IPC Plugin
## Architecture, technical specification, and implementation plan for the first working version

**Document status:** Implementation plan (revision 2)  
**Prepared:** 23 October 2026 · **Revised:** 24 October 2026  
**Target:** KiCad 11 first-class (developed against pinned 10.99 nightly), KiCad 10.0.x fallback · Rust · Model Context Protocol (MCP)  
**First milestone:** A working Rust MCP server can inspect and modify a KiCad 11 schematic through the official IPC API (headless `kicad-cli api-server` and the live Schematic Editor), verify a small test circuit by read-back, and degrade to a documented, reduced capability set on KiCad 10.

### Revision 2 summary

| Change | Reason |
|---|---|
| KiCad 11 is the primary target; KiCad 10 is a supported fallback with a reduced capability set | KiCad 11 is not released yet (expected ~Q1 2027, not committed). The 10.99 nightly provides the schematic IPC API, headless `api-server`, and IPC jobs today. KiCad 10 has no schematic IPC commands. |
| Introduced a capability-probed `KiCadBackend` layer and three session providers (Headless, GUI-plugin, Direct-socket) | One set of MCP tool contracts across KiCad 10 and 11; the backend is chosen by runtime probing, never encoded in tool schemas. |
| Headless provider moved from "later" to the first IPC path | `kicad-cli api-server` is deterministic, needs no GUI plugin lifecycle, and is testable in CI. It removes plugin-lifetime risk from the critical path. |
| Windows is first-class from day one | Primary development machine is Windows 11; NNG `ipc://` maps to named pipes there. |
| Tools take explicit document targets; "active document" is an observation, not an implicit target | KiCad 10 has no typed active-document query; explicit targets are also safer with multiple open projects. |
| Result envelope gains `result_state` and `provenance` | A check that did not run is never reported as passing; every result states where its data came from (KiCad IPC, project file, CLI). |
| Added `kicad-sexpr` (file backend) and `kicad-cli` runner crates; added mock IPC server for tests | KiCad 10 fallback, offline verification, fixtures, and CI without a running KiCad. |
| Phase plan re-sequenced (§14) | Spike both versions first; v11 IPC path before v10 file fallback. |

---

### Revision 3 summary (after Phases 0–7)

The plan below is kept as written where it still holds. Where building it led elsewhere, the sections were updated and the full list is in [architecture.md](architecture.md#changes-from-the-original-plan): the plugin is a registrar, not a bridge; `AnySession` replaced the backend trait; there is no direct-socket provider; the file backend edits only the text a plan names instead of keeping a full syntax tree; a layout engine and a release pipeline were added.

---

## 1. Executive decision

Build the first version as separate layers with clear process boundaries:

1. **`kicad-ai-mcp`**: a Rust MCP server launched by an MCP host over stdio. It exposes validated, intent-oriented tools, selects a backend by capability probing, and owns the headless `kicad-cli api-server` child process when that provider is used.
2. **`kicad-ai-plugin`**: a Rust executable IPC plugin run from a KiCad 11 Schematic (or PCB) Editor toolbar button. It receives the KiCad-provided socket and instance token, confirms the instance answers, records it in a user-private registry, and exits. The MCP server then connects to exactly that instance with the token pinned (revision 3: registrar instead of a relaying bridge; see `docs/tasks.md` 4.1). **KiCad 11 only**: KiCad 10 has no schematic plugin scope.
3. **`kicad-domain`**: a shared library containing the circuit intermediate representation, tool argument types, validation, layout rules, operation planning, and the capability model. It is a library, not a process, and has no KiCad transport types.

Supporting library crates: `kicad-ipc` (protobuf + NNG adapter), `kicad-sexpr` (lossless `.kicad_sch` engine for the KiCad 10 fallback and offline reads), `kicad-cli-runner` (ERC/export/diff jobs).

Core rules:

- The MCP server does not guess which KiCad instance to mutate. It either owns the instance (headless provider), takes it from the registration the user made in the editor (gui provider, token pinned), or works on a file no editor has open (file provider).
- All requests to one KiCad instance are serialized through one IPC connection. KiCad handles API requests synchronously on its event loop; multiple connections give no throughput and risk ordering problems.
- Tool contracts never mention a backend. A result states its backend in `provenance`.

### First milestone boundaries

**Included**
- KiCad 11 (pinned nightly) schematic IPC: open/inspect a schematic, place library symbols, set fields, create wires and local labels, read the schematic netlist, save.
- Headless provider (`kicad-cli api-server`) as the first IPC path; the GUI provider (plugin registrar) as the second.
- KiCad 10 fallback: read-only inspection via file parsing, ERC and netlist via `kicad-cli`, and — in a later phase of the milestone — file-based mutation of documents not open in an editor.
- Rust MCP server using the official Rust MCP SDK (`rmcp`) over stdio.
- A private, current-user-only registry for editor registrations (revision 3: no bridge protocol).
- A deterministic two-resistor / three-net test schematic, verified on both KiCad versions.
- Integration tests, a mock IPC server for CI, and a repeatable local workflow.

**Not included in the first milestone**
- AI model hosting or orchestration.
- PCB layout, footprint placement, routing, manufacturing release.
- Part selection, distributor lookup, datasheet interpretation, simulation.
- Custom-symbol authoring, hierarchical generation, variants, buses, complex multi-unit symbols, placement optimization.
- Remote network access to the MCP server or the editor registry.
- 3D enclosure design (suite roadmap; see §16.1).

---

## 2. KiCad API facts and version constraints

### 2.1 What each version provides (verified 24 Oct 2026)

| Capability | KiCad 10.0.x (stable) | KiCad 11 / 10.99 nightly |
|---|---|---|
| IPC plugin scopes | PCB Editor only | Schematic Editor and PCB Editor |
| Schematic IPC commands | None. `schematic_commands.proto` defines no messages; `schematic_types.proto` has 7 types (Line, Text, 4 label kinds) | `GetSchematicHierarchy`, `GetSchematicNetlist`, `PlaceSymbolFromLibrary`; ~44 schematic types (symbols, pins, fields, junctions, no-connects, sheets) |
| Generic editor commands for schematics | `GetOpenDocuments`, selection commands. No typed active-document query | Generic `CreateItems` / `UpdateItems` / `DeleteItems` / `GetItems` with schematic types, commit/undo |
| Jobs (ERC/export) over IPC | No; use `kicad-cli` | `schematic_jobs.proto`, `board_jobs.proto` |
| Headless | No | `kicad-cli api-server [--socket PATH] [PROJECT_OR_FILE]` |
| Other new protos | — | `library_commands`, `cross_probe_commands`, `variant_commands`, `schematic_rules` |
| SWIG `pcbnew` | Present, deprecated | Removed |
| `kicad-cli sch` | `erc`, `export {bom,dxf,hpgl,netlist,pdf,ps,python-bom,svg}`, `upgrade` | adds `diff` (JSON/text/PNG/SVG), `downgrade`, `import`, `export png` |
| Other `kicad-cli` | `fp`, `jobset`, `pcb`, `sch`, `sym`, `version` | adds `api-server`, `gerber`, `import`, `mergetool`, `project` |

The IPC protobuf API is stable under protobuf rules: existing messages and fields keep their meaning; new ones are added; deprecated items survive at least one major version. Practical consequence: **one generated proto set (from the pinned KiCad 11 commit) can talk to both versions**, with KiCad 10 answering newer requests with `AS_UNHANDLED` / `AS_UNIMPLEMENTED`. This must be confirmed in the Phase 0 spike.

`ApiRequestHeader.kicad_token` is optional. If a non-empty value is sent and does not match, KiCad rejects the request with `AS_TOKEN_MISMATCH`. Clients use it to pin a specific instance after the first reply.

### 2.2 Version policy

- **Tier 1 — KiCad 11.** Developed against a pinned nightly build until 11.0 is released, then pinned to 11.0.x. All MVP tools are implemented on the IPC backend.
- **Tier 2 — KiCad 10.0.x (≥ 10.0.7 tested).** Reduced capability set (§3.3). Must never be silently presented as equivalent to Tier 1; `kicad_status` reports the capability matrix.
- **Nightly drift.** Nightly protos can change. The pinned build hash, proto commit, and checksum are recorded in `docs/kicad-api-compatibility.md`. Updating the pinned nightly is a deliberate change with the Phase 0 probe suite re-run.
- **File formats.** KiCad 10 cannot open files written by KiCad 11 (newer `(version …)` token). The file backend must preserve a file's format version, refuse to write a version newer than it understands, and never upgrade a file implicitly. `kicad-cli sch downgrade` (11) exists but is out of scope for automatic use.

### 2.3 Development environment (validated 24 Oct 2026)

| Item | Value |
|---|---|
| OS | Windows 11 Pro 26H2 (build 26300), x64 |
| KiCad nightly | `10.99.0-5896-gd033635628`, built 23 Oct 2026, `D:\Program Files\KiCad\10.99` |
| KiCad stable | `10.0.7`, built 6 Oct 2026, `C:\Program Files\KiCad\10.0` |
| Environment | Machine-level `KICAD_HOME = D:\Program Files\KiCad\10.99`; `%KICAD_HOME%\bin` on machine `Path` (REG_EXPAND_SZ). Processes started before the change do not see it. |
| API server setting | 10.99: `api.enable_server = true`. 10.0: API server enabled by the user (24 Oct 2026). |
| User documents | `C:\Users\bkraj\OneDrive\Documents\KiCad\{10.0,10.99}` — OneDrive-redirected. Plugin directories live here; resolve the documents path at runtime, never hardcode it. |

The proto commit to vendor for Tier 1 is the nightly's git hash, `d033635628`.

### Official references
- KiCad IPC API: <https://dev-docs.kicad.org/en/apis-and-binding/ipc-api/>
- Add-on development, plugin manifest, environment variables: <https://dev-docs.kicad.org/en/apis-and-binding/ipc-api/for-addon-developers/>
- Schematic API reference (kicad-python, KiCad 11): <https://docs.kicad.org/kicad-python/schematic.html>
- KiCad source protos: <https://gitlab.com/kicad/code/kicad/-/tree/master/api/proto>
- Official KiCad Python IPC wrapper (reference for message usage): <https://gitlab.com/kicad/code/kicad-python>
- KiCad API plugin JSON Schema: <https://go.kicad.org/api/schemas/v1>
- Official Rust MCP SDK: <https://github.com/modelcontextprotocol/rust-sdk>

---

## 3. System architecture

```mermaid
flowchart TB
    LLM["AI assistant<br/>(MCP host)"]

    subgraph MCP["kicad-ai-mcp: one process per assistant, stdio"]
        TOOLS["MCP tools<br/>session_*  sch_*<br/>plan store, idempotency keys,<br/>result envelope"]
    end

    DOMAIN["kicad-domain<br/>circuit spec, plans, layout engine,<br/>readability checks, capabilities, results"]
    ANY["kicad-session<br/>AnySession"]

    subgraph API["API sessions: KiCad 11"]
        SESS["Session<br/>headless or gui"]
        IPC["kicad-ipc<br/>protobuf over a native NNG client"]
        REG[("instance registry<br/>one JSON file per editor")]
    end

    subgraph FILE["File sessions: KiCad 10, or no KiCad running"]
        FS["FileSession<br/>verified copy, atomic replace"]
        SEXPR["kicad-sexpr<br/>parser, library reader,<br/>in-place writer"]
    end

    CLIR["kicad-cli-runner<br/>discovery, ERC, netlist, PNG"]
    CLI["kicad-cli"]
    HL["kicad-cli api-server<br/>headless, a child of the MCP process"]
    GUI["KiCad 11 editor"]
    PLUGIN["kicad-ai-plugin<br/>toolbar button: registers the editor"]
    FILES[("schematic files")]

    LLM <-->|"MCP JSON-RPC over stdio"| TOOLS
    TOOLS --> DOMAIN
    TOOLS --> ANY
    ANY --> SESS
    ANY --> FS
    SESS --> IPC
    IPC <-->|"private named pipe or socket"| HL
    IPC <-->|"socket and token from the registry"| GUI
    GUI -->|"runs it on a button press"| PLUGIN
    PLUGIN -->|"writes socket, token, version"| REG
    SESS -->|"reads"| REG
    FS --> SEXPR
    SEXPR <--> FILES
    SESS --> CLIR
    FS --> CLIR
    CLIR --> CLI
    CLI --> FILES
    HL <--> FILES
    GUI <--> FILES
```

The same picture with the crate graph, the registration flow, and both apply sequences is in [architecture.md](architecture.md).

### 3.1 Session providers

`KiCadSessionProvider` produces one serialized IPC session bound to one KiCad instance.

| Provider | KiCad | How the instance is chosen | Use |
|---|---|---|---|
| `Session`, headless (`provider: headless`) | 11 | MCP spawns `kicad-cli api-server --socket <private path> <project>` with a fixed argument array; the socket path is in a private runtime directory | **First IPC path.** Deterministic automation, CI, and generation of designs not open in a GUI |
| `Session`, GUI (`provider: gui`) | 11 | The user presses the "KiCad AI: connect" toolbar button; the plugin registers the instance (socket + token) and the MCP server connects to it with the token pinned | Live co-editing with the user; changes appear in the editor with undo |
| `FileSession` (`provider: file`) | 10 and 11 | An explicit `.kicad_sch` path that no editor has open | KiCad 10 fallback, and any generation that needs no running KiCad; edits verified in a copy, then replaced atomically |

A `DirectSocketProvider` for attaching to a KiCad 10 editor was planned and not built: KiCad 10's API reports open documents by filename only and only the first KiCad on Windows gets an endpoint, so the lock file is the guard against an open editor instead.

Rules:
- Never attach to a socket the current user does not own.
- Never open a raw stream connection to an NNG endpoint for discovery. A connection that does not complete the NNG handshake can wedge KiCad's server; Windows named pipes also consume a server instance.
- The headless child is owned by the MCP process: killed on MCP exit, deadline, or provider shutdown; never orphaned.
- Headless sessions do not preload libraries. Session start issues `LoadAllLibraries` (symbol type) and the session reports "libraries loading" until the libraries a plan needs are available; placement before that fails with `AS_BAD_REQUEST` ("symbol … not found"). GUI sessions must not send it: the GUI preloads libraries and answers `AS_UNIMPLEMENTED`.
- A headless session does not need a fixture file: start `api-server` without a file, then `CreateDocument` (new schematic in memory), or `OpenDocument` for an existing one. `SaveDocument`, `RevertDocument`, `GetDocumentModifiedState`, and `CloseDocument` / `CloseAllDocuments` manage the document lifecycle; one server can switch documents without a restart.
- The headless server prints no readiness line when its output is redirected. The provider detects readiness by polling connect + `Ping` within a deadline.
- Commits are available in headless sessions whose document was opened with `OpenDocument`/`CreateDocument` (a server started with a file argument has none), and in the GUI. Inside an open commit a newly placed symbol is not yet part of the document, so an apply uses **two commits**: (1) place the symbols, (2) set field values and add wires and labels. One apply is therefore two undo steps. A failed step's commit is dropped, and anything an earlier step created is deleted with `DeleteItems`. Sessions without commits record every created KIID and delete them on failure. The result is `rejected` with `mutation_state: not_applied` when the undo succeeded, `unknown` otherwise.
- KiCad's IPC export jobs (netlist, plot) read the **saved file**, not the in-memory document. ERC, rendering, and netlist export therefore work on a `SaveCopyOfDocument` snapshot in a private folder, processed with `kicad-cli`; the user's file is not touched.
- Commits are owned by the request's `client_name`; no other client can end them. Each session uses one stable `client_name` for its lifetime, and every error path inside an apply ends its open commit with `CMA_DROP` before reporting.
- On Windows only one GUI instance gets an API endpoint: with two KiCad GUIs running (one 10.99, one 10.0.7), the second had no reachable socket at all. `session_status` must report which instance answered (version + token) and never assume the endpoint belongs to the KiCad the user means; the plugin provider (which receives the socket from KiCad) and the headless provider (own socket) avoid the ambiguity.
- One provider per target document at a time. A document open in a GUI must not also be mutated through a headless server or the file backend.

### 3.2 Sessions and the capability model

```text
enum AnySession { Ipc(Session), File(FileSession) }   // same operations on both
```

| Part | KiCad | Provides |
|---|---|---|
| `Session` over `kicad-ipc` | 11 | Schematic read/mutate, hierarchy, netlist, save, document lifecycle |
| `FileSession` over `kicad-sexpr` | 10, 11 | Schematic read from the file; guarded mutation of a document no editor has open |
| `kicad-cli-runner` (used by both) | 10, 11 | ERC (`--format json`), netlist, PNG (11 only) and other exports, `sch upgrade` |

Capabilities are probed when an API session opens (`GetVersion`, then a harmless request per capability family; `AS_UNHANDLED` / `AS_UNIMPLEMENTED` → unsupported) and fixed for a file session. If KiCad answered and rejected a request, the operation stops and reports the KiCad error; it never retries through another backend.

### 3.3 Capability matrix

The observed matrix, per provider, is in [capability-matrix.md](capability-matrix.md); a test keeps it listing every capability.

### 3.4 Process ownership

| Component | Process model | Responsibility | Must not do |
|---|---|---|---|
| MCP server | One process per MCP host session | Tools, validation, policy, plans, backend selection, owning the headless child | Write non-protocol text to stdout; attach to unowned sockets; fall back after a KiCad rejection |
| Headless API server | Child of the MCP server | Load the target project, serve IPC | Outlive the MCP process; listen on a non-private path |
| Plugin (registrar) | Launched by a KiCad 11 editor action; exits when done | Write the editor's socket, token, version, and open projects to the instance registry; show a confirmation | Serve requests; stay running; log the token |
| `kicad-ipc` | Library | Envelope/protobuf, NNG, deadlines, status mapping | Leak protobuf types to MCP/domain |
| `kicad-sexpr` | Library | Parse with source positions, read symbol libraries and schematics, add a plan to a schematic by inserting text | Re-print a file; change its format version |
| `kicad-cli-runner` | Short-lived children | ERC/export/diff with fixed argument arrays, JSON reports | Use a shell; treat exit 0 as functional correctness |
| `kicad-domain` | Library | IR, plans, layout engine and readability checks, validation, capabilities | Depend on transport or UI state |

---

## 4. Repository layout

Single Cargo workspace. No microservices or containers until the core works.

```text
thingsvu-tools/
├── Cargo.toml  Cargo.lock  rust-toolchain.toml  rustfmt.toml
├── README.md  LICENSE
├── .github/workflows/{ci,release}.yml
├── docs/
│   ├── README.md
│   ├── architecture.md                 # current diagrams and the changes from this plan
│   ├── capability-matrix.md            # per provider
│   ├── kicad-ai-mcp-design-plan.md     # this file
│   ├── kicad-api-compatibility.md      # pinned builds, proto commit, probe results
│   ├── releasing.md
│   ├── tasks.md                        # phase task list with decisions to review
│   └── examples/                       # circuit specs: two-resistor chain, regulator, NE555
├── plugin-package/{plugin.json,icons/}
├── crates/
│   ├── kicad-ai-mcp/                   # bin + lib: MCP stdio server
│   │   └── src/{main,lib,server,plans,envelope}.rs
│   ├── kicad-session/                  # lib: Session (headless, gui), FileSession, AnySession
│   │   └── src/{any,session,headless,file,mutate,schematic,registry,capability_map,errors,job}.rs
│   ├── kicad-ai-plugin/                # bin: KiCad 11 plugin that registers the editor
│   ├── kicad-domain/                   # lib: circuit spec, plans, layout, readability, validation
│   │   └── src/{circuit,geometry,ids,layout,plan,readability,schematic,validation,capability,result}.rs
│   ├── kicad-ipc/                      # lib: protobuf + native NNG client (protos vendored)
│   ├── kicad-sexpr/                    # lib: parser, symbol libraries, schematic reader, writer
│   │   └── src/{node,sym,sch,emit}.rs
│   ├── kicad-cli-runner/               # lib: kicad-cli discovery and jobs
│   ├── kicad-ipc-mock/                 # dev lib: fake KiCad API server on a real pipe/socket
│   └── kicad-fault-tests/              # failure injection: fake kicad-cli, misbehaving API servers
├── tests/fixtures/v11/                 # KiCad-authored files
└── scripts/
    ├── install.ps1  uninstall.ps1      # for users (shipped in the release zip)
    ├── package.ps1  test-installer.ps1 # release packaging and its smoke test
    ├── install-plugin.ps1  uninstall-plugin.ps1   # for developers: plugin only, from a build
    └── probe-kicad.ps1                 # Phase 0 capability probe
```

Shell (`.sh`) equivalents are added when Linux/macOS are supported.

**License:** the project is licensed GPL-3.0-or-later, matching KiCad and its `.proto` files. Vendored protos keep their original headers; third-party dependencies must be GPL-3.0-compatible (checked with `cargo deny`).

---

## 5. Technology specification

### 5.1 Rust and workspace
- Stable Rust pinned in `rust-toolchain.toml`; `Cargo.lock` committed.
- `tokio`, `serde`/`serde_json`, `tracing`/`tracing-subscriber`, `thiserror` (libraries), `anyhow` (binaries only).
- `rmcp` (official Rust MCP SDK), exact version pinned. Use structured tool output (`structuredContent` + `outputSchema`) where the pinned release supports it.
- `schemars` (or the rmcp-integrated equivalent) to generate tool input schemas from Rust types.

### 5.2 KiCad IPC adapter (`kicad-ipc`)
- `prost` / `prost-build` code generated from the vendored protos. Record commit and SHA-256 per file.
- NNG transport implemented natively in Rust (decision from Phase 0): KiCad needs only the REQ0 side of NNG's ipc transport, which is small and was verified on the wire — an 8-byte SP header exchange (`00 'S' 'P' 00 00 30 00 00` ↔ `… 00 31 …`), then frames of `0x01` + u64 big-endian length + body, where the body starts with a 4-byte request ID (high bit set) that the reply echoes. Windows: `tokio` named pipe `\\.\pipe\<socket path>`; Unix: Unix domain socket at the socket path. One outstanding request per connection. This removes the C NNG library and CMake from the build. Keep it behind a `Transport` trait so a binding to the C library can be swapped in if a KiCad version needs NNG features beyond this subset.
- Typed envelope with a stable per-session `client_name` (`thingsvu-kicad-ai-<session id>`; commits are owned by it) and the `kicad_token` pinned from the first reply (a wrong token is refused with `AS_TOKEN_MISMATCH`).
- Map every `ApiStatusCode` to a typed error: `AS_BUSY`/`AS_NOT_READY` → retryable; `AS_UNHANDLED`/`AS_UNIMPLEMENTED` → `FEATURE_UNAVAILABLE`; `AS_TOKEN_MISMATCH` → `KICAD_INSTANCE_CHANGED`; `AS_BAD_REQUEST` → non-retryable.
- One serialized request channel per session with a bounded queue; a deadline on every request; no blind retries of mutations.
- Unit conversion (mm ↔ KiCad internal units) lives only here; the pinned proto definitions are the authority on units.
- Protobuf `Any` payloads are dispatched by full type URL. Unknown types are an error, never decoded as empty.

### 5.3 File backend (`kicad-sexpr`)
- Lossless by construction: the parser keeps each node's byte span and the writer edits only the text a plan names (new library symbols at the end of `lib_symbols`, new items before `sheet_instances`, and — since Phase 8 — deleting items and changing a symbol's `(at …)`, `(mirror …)`, and its fields' positions), so every other byte of the file stays as it was.
- UUIDs preserved; new objects get v4 UUIDs. File UUIDs are the same KIIDs the IPC API returns, so object identities survive a backend switch.
- Atomic write: temp file in the same directory → flush/fsync → rename. Capture a content hash at read; refuse to write if the file changed since (`FILE_CHANGED_ON_DISK`).
- Refuse mutation if the document is open in any editor: KiCad lock file (`~<name>.kicad_sch.lck`), `GetOpenDocuments` on any reachable instance, or a headless session holding it.
- Preserve the file's `(version …)`; refuse unknown newer versions.
- Validate every write before it replaces the file: the result is written to a copy that `kicad-cli sch export netlist` must load, with exactly the planned nets (`kicad-cli`'s netlister omits power symbols, so nets are compared by real parts and by name). Built in Phase 6, as `provider: "file"`.

### 5.4 CLI runner (`kicad-cli-runner`)
- Discover installs from configuration first, then `KICAD_HOME`, then standard install directories. Each install records its `kicad-cli version` output. Pair the CLI with the KiCad version of the target session; never run the 10.0 CLI on a file written by 11.
- Fixed argument arrays via `std::process::Command`; no shell.
- Prefer machine-readable output (`sch erc --format json`, `sch diff` JSON on 11). Capture exit code, stdout, stderr, and report path separately.
- Version-specific argument tables live here (flags change between majors).

### 5.5 MCP server
- Transport: stdio. All diagnostics on stderr or a private log file.
- Register only implemented and tested tools. Tools whose capability is unavailable on the current session are still listed but return `FEATURE_UNAVAILABLE` with the feature name, so agent behavior does not depend on hidden tools.
- JSON Schema (Draft 2020-12) for every input, enforced at dispatch; domain validation still runs in handlers.
- Stable error codes: `NO_SESSION`, `BRIDGE_DISCONNECTED`, `KICAD_BUSY`, `KICAD_INSTANCE_CHANGED`, `FEATURE_UNAVAILABLE`, `DOCUMENT_NOT_OPEN`, `DOCUMENT_OPEN_ELSEWHERE`, `FILE_CHANGED_ON_DISK`, `STALE_PLAN`, `SYMBOL_NOT_FOUND`, `INVALID_GRID_COORDINATE`, `INVALID_ARGUMENT`, `AMBIGUOUS_TIMEOUT`, `ERC_UNAVAILABLE`, `READ_ONLY_MODE`.
- Group tools by prefix (`session_`, `sch_`, later `pcb_`, `lib_`, `mech_`) so tool groups can be enabled on demand later via `notifications/tools/list_changed`. Do not hide tools behind a generic `execute_tool` proxy.

### 5.6 GUI instance registry (replaces the bridge protocol)
- The plugin writes one JSON record per KiCad endpoint to a user-private folder (`%LOCALAPPDATA%\thingsvu\kicad-ai\instances` on Windows; `$XDG_STATE_HOME` or `~/.local/state` elsewhere): socket path, instance token, KiCad version, open projects, timestamp, plugin version.
- The MCP server lists records, checks each by connecting with the token pinned (`GetVersion`), and removes records whose instance no longer answers.
- The token is never included in tool results or logs; `session_status` shows only version and projects.
- No local listener of our own: the only endpoints are KiCad's.

---

## 6. Executable plugin specification (KiCad 11)

### 6.1 Actions
- **KiCad AI: connect** (`register`), toolbar button, scopes `schematic` and `pcb`.
- **KiCad AI: disconnect** (`unregister`), no toolbar button by default.

The action process exits immediately, so KiCad's handling of long-running executable actions does not matter.

### 6.2 Manifest
`plugin-package/plugin.json`, copied with the binary and toolbar icons by `scripts/install-plugin.ps1`. The `entrypoint` must be relative to the plugin folder: KiCad skips actions whose entrypoint is an absolute path (`common/api/api_plugin.cpp`), without any visible message. It is valid against `api.v1.schema.json` from the pinned build. KiCad 10 does not support the `schematic` scope, so the installer refuses versions below 10.99.

### 6.3 Behavior of `register`
1. Read `KICAD_API_SOCKET` (strip `ipc://`) and `KICAD_API_TOKEN`; if missing, explain that the plugin runs from a KiCad toolbar button.
2. Connect, pin the token, `GetVersion`, and `GetOpenDocuments` for schematics and boards.
3. Write the registry record atomically.
4. Print one line (KiCad shows it in its message area) and exit.

### 6.4 User experience
- Report results and failures as one stdout line (KiCad 10.0.1+ shows executable-plugin console output in its message area; confirm on 11).
- Never show modal UI on the request path.
- Group mutations into commits: an apply is two undo steps (placement, then values and connections; see §3.1).
- Never write outside the project directory unless the user supplied the destination.

---

## 7. MCP tool design

### 7.1 First tool set

All document-targeting tools take an explicit `project` (path to `.kicad_pro`) and, where relevant, `schematic` (project-relative `.kicad_sch` path) and `sheet_path`.

| Tool | Capability | Purpose |
|---|---|---|
| `session_status` | `session.status` | Providers, sessions, KiCad versions, capability matrix, read-only mode |
| `session_open` | — | Open a target project with a chosen or automatic provider (`headless`, `plugin`, `direct`, `file`); returns a session ID |
| `session_list_documents` | `session.open_documents` | Open documents per session (observation; includes GUI-active info when KiCad exposes it) |
| `sch_list_symbols` | `sch.read_symbols` | UUID, reference, value, library ID, position (mm), sheet path, with `provenance` |
| `sch_get_symbol_pins` | `sch.read_pins` | Pin number/name/type and world position for one symbol (UUID or unambiguous reference) |
| `sch_get_netlist` | `sch.netlist` | Nets with connected (symbol UUID, reference, pin) tuples |
| `sch_plan_changes` | — | Validate a `CircuitSpec` or operation list against a snapshot; returns plan ID, operations, warnings |
| `sch_apply_plan` | `sch.place_symbol` etc. | Apply a plan with `confirm: true`; one commit; returns created IDs and read-back verification |
| `sch_save` | `sch.save` | Save the target schematic |
| `sch_run_erc` | `sch.erc` | ERC with parsed findings and report path |
| `sch_export_netlist` | `sch.netlist` | Export to a project-relative path |
| `sch_render` | `sch.render` | SVG/PNG snapshot for visual inspection |
| `sch_create_demo_circuit` | via plan | The two-resistor fixture as plan → apply → verify |

Built in Phase 8: `sch_review`, `sch_tidy_plan`, `sch_set_field`, `sch_describe_layout`, `sch_arrange_plan`, `sch_reconnect_plan`. Later: `sch_add_power_symbol`, `sch_remove_item`, `sch_diff`, `session_get_installation_info`.

### 7.2 Tool design rules
1. Stable IDs (KIIDs/UUIDs) internally; references are display values.
2. Pins by symbol identity plus pin number; never infer from ambiguous names.
3. Plan and apply are separate. A plan records session, document identity, and a document revision/hash; it is `STALE_PLAN` if any change.
4. Idempotency key on every apply; a repeated key returns the earlier result.
5. Success is derived from read-back, never from the request echo.
6. Prefer high-level batches. In GUI sessions one apply = one KiCad commit = one undo step where KiCad supports commits; in headless sessions (no commit support) an apply is a recorded sequence with cleanup on failure (§3.1).
7. Bound counts, coordinates, payload sizes, and durations.
8. Return changed IDs and a diff summary.
9. Same contract on every backend; tests in `backend_contract.rs` run identical assertions on IPC and file backends.

### 7.3 Example argument schemas

`sch_apply_plan`:

```json
{
  "type": "object",
  "required": ["session_id", "plan_id", "confirm", "idempotency_key"],
  "properties": {
    "session_id": {"type": "string", "maxLength": 64},
    "plan_id": {"type": "string", "maxLength": 64},
    "confirm": {"const": true},
    "idempotency_key": {"type": "string", "minLength": 8, "maxLength": 128}
  },
  "additionalProperties": false
}
```

Operation (inside a plan), `add_wire`:

```json
{
  "type": "object",
  "required": ["op", "start_mm", "end_mm"],
  "properties": {
    "op": {"const": "add_wire"},
    "start_mm": {"$ref": "#/$defs/point_mm"},
    "end_mm": {"$ref": "#/$defs/point_mm"},
    "sheet_path": {"type": "string", "maxLength": 512}
  },
  "additionalProperties": false
}
```

Public coordinates are millimetres; endpoints must be on the connection grid with non-zero length.

### 7.4 Result envelope

```json
{
  "ok": true,
  "result_state": "done",
  "result": { "...": "tool-specific" },
  "provenance": [
    {"origin": "kicad_ipc", "kicad_version": "10.99.0-5896-gd033635628", "provider": "headless", "document": "demo.kicad_sch", "revision": "sha256:…"}
  ],
  "warnings": [],
  "correlation_id": "req-000042"
}
```

`result_state` is one of:

| State | Meaning |
|---|---|
| `done` | Operation and its verification finished |
| `partial` | Lists applied, failed, and not-attempted items |
| `rejected` | Nothing was applied; the error explains why |
| `unknown` | A change may have been applied; read back before retrying |
| `not_run` | A required check or feature could not run; never reported as a pass or as zero findings |

Error example:

```json
{
  "ok": false,
  "result_state": "unknown",
  "error": {
    "code": "AMBIGUOUS_TIMEOUT",
    "message": "KiCad did not reply before the deadline after the commit was sent.",
    "retryable": false,
    "mutation_state": "unknown",
    "correlation_id": "req-000042"
  }
}
```

`provenance.origin` is one of `kicad_ipc`, `project_file`, `kicad_cli`. A result combining sources lists each.

---

## 8. Domain model and coordinate/connectivity rules

### 8.1 Initial domain types
- `CircuitSpec`, `ComponentSpec` (logical ID, library ID, reference preference, value, fields, optional footprint and MPN/LCSC fields for later part tooling), `PinRef`, `NetSpec`.
- `OperationPlan`: ordered operations with preconditions, expected effects, document revision, and reconciliation notes.
- `DocumentSnapshot`: identity, revision/hash, symbols, pins, wires, labels.
- `CapabilitySet`: probed capabilities per session.
- `ResultState` and `Provenance`.
- `ValidationFinding`: severity, code, affected objects, explanation, suggested next step.

Never use `R1` as the sole identity of a component.

### 8.2 Grid and geometry
- Public coordinates in mm; the standard connection grid is 50 mil (1.27 mm).
- Snap or reject off-grid endpoints with an actionable error.
- Pin world positions: on KiCad 11, read from IPC; on the file backend, compute from library-local pin positions plus the symbol transform (rotation, mirror). Both paths share one transform implementation in `kicad-domain`, cross-checked against IPC read-back in tests.
- Re-read connectivity after wiring; geometry is not proof of connection.
- Deterministic simple grid layout for the MVP.

### 8.3 Electrical intent vs graphical layout
A circuit says `R1.2` and `R2.1` are on `MID`; the layout engine chooses coordinates; the backend creates wires and labels.

---

## 9. First working circuit and end-to-end behavior

```text
INPUT ── R1 (10 kΩ) ── MID ── R2 (10 kΩ) ── OUTPUT
```

`Device:R` symbols; `INPUT`, `MID`, `OUTPUT` local labels; 1.27 mm grid. Fixture: `examples/two-resistor-series-chain.json`.

### Expected agent/tool sequence (KiCad 11, headless)
1. `session_status` → KiCad 11 available, headless provider, capability matrix.
2. `session_open` with the disposable test project, provider `headless`.
3. `sch_create_demo_circuit` in plan mode → plan ID and operations.
4. `sch_apply_plan` with `confirm: true` and an idempotency key → one commit.
5. Read-back of symbols and `GetSchematicNetlist`.
6. `sch_save`, `sch_export_netlist`, `sch_run_erc`.
7. Test compares the exported netlist graph to the fixture.

### KiCad 10 fallback sequence
Same tool calls with `session_open` provider `file` on a project not open in any editor. Mutation goes through `kicad-sexpr`; netlist and ERC through the 10.0 `kicad-cli`. `provenance.origin` is `project_file` / `kicad_cli`.

### Acceptance criteria

**KiCad 11 (Tier 1)**
- [ ] Headless provider starts, connects, probes, and stops cleanly; no orphan process after MCP exit or kill.
- [ ] `session_status` succeeds through a real MCP client over stdio; no non-protocol stdout.
- [ ] Symbol inventory matches an existing KiCad-authored fixture.
- [ ] Demo circuit is created through IPC in one commit; read-back verifies values, references, labels, and topology.
- [ ] Retrying an apply with the same idempotency key creates no duplicates.
- [ ] A timeout after a commit returns `unknown` and is reconciled by read-back, not blind retry.
- [ ] Saved project reopens in the KiCad 11 GUI without a repair prompt.
- [ ] Plugin provider: the Schematic Editor action appears, connects to the right instance, and the same demo works live with one undo step.
- [ ] Closing/restarting KiCad yields `BRIDGE_DISCONNECTED` / `NO_SESSION`, never a hung tool call.
- [ ] Logs contain no API token.

**KiCad 10 (Tier 2)**
- [ ] `session_status` reports KiCad 10 with the reduced capability matrix.
- [ ] Read tools work on saved files with `provenance.origin = project_file`.
- [ ] ERC and netlist run through the 10.0 `kicad-cli` and are parsed.
- [ ] Mutation is refused with `DOCUMENT_OPEN_ELSEWHERE` while the schematic is open in KiCad 10.
- [ ] Demo circuit via the file backend: the result opens in KiCad 10.0.7 without a repair prompt, and its netlist matches the fixture.
- [ ] Byte-level round trip of unmodified KiCad-authored fixtures.

---

## 10. Request sequencing, timeouts, and recovery

```text
MCP tool call
  -> schema + domain validation
  -> plan (snapshot + revision)
  -> backend selection by capability
  -> bounded per-session queue
  -> single IPC client / file transaction / CLI job
  -> typed result or typed error
  -> read-back and reconciliation for mutations
```

| Operation | Retry policy |
|---|---|
| Read-only | Bounded retry on `AS_BUSY` / `AS_NOT_READY` |
| Save / export | Retry only after checking document/job state |
| Create / update / delete | Never blindly retried; read back and reconcile using idempotency key and expected object IDs |
| CLI job | Deadline; separate stdout/stderr; parse report when present |
| Headless server start | Deadline for readiness; kill and report on failure |

Starting values (configurable; record observed behavior per build): bridge request 10 s; busy backoff 200 ms / 500 ms / 1 s; tool max 30 s (export/ERC longer); max 50 operations and 200 wire segments per plan.

Plans are bound to session, document identity, and revision; any mismatch returns `STALE_PLAN`.

---

## 11. Validation strategy

1. Tool-input validation (schema, identifiers, ranges, sizes).
2. Domain validation (duplicate IDs, reference collisions, pin refs, zero-length wires, undefined net members).
3. Geometry validation (grid, transforms, deterministic routing).
4. IPC status validation.
5. Read-back validation (symbols and netlist after apply).
6. ERC (IPC job or `kicad-cli sch erc --format json`).
7. Export validation (parse exported netlist; compare to intent).
8. Reopen validation in the GUI of the same KiCad version.
9. Cross-version check: a file written by the KiCad 10 path opens in KiCad 10.0.7; a file written by KiCad 11 is never handed to the KiCad 10 path.
10. Electrical/design validation: later phase. Zero ERC errors is not proof of correctness.

### Testing levels
- **Unit:** snapping, transforms, reference allocation, schema validation, plan staleness, error mapping, capability selection.
- **Protocol:** envelope encode/decode against `kicad-ipc-mock` (inproc NNG), status-code mapping, token redaction, bridge framing.
- **File backend:** round trip of KiCad-authored fixtures (v10 and v11), atomic write, stale-target refusal, lock refusal.
- **MCP:** tool listing, invalid inputs, structured results, stdio purity, disconnect handling.
- **Backend contract:** identical assertions on IPC and file backends for the demo circuit.
- **Live integration** (`KICAD_LIVE=1`, serial): pinned nightly headless and GUI; KiCad 10.0.7 GUI and CLI.
- **Failure injection:** kill KiCad during read/mutation; malformed bridge message; second KiCad instance; token mismatch; duplicate apply; file changed between plan and apply.
- **Guard-removal tests:** for each safety guard, a test that is shown to fail when the guard is disabled.

CI: unit/protocol/file/MCP tests on every push (Windows runner first). Live tests on the developer workstation; later on CI using official KiCad Docker images (stable and nightly) for CLI and headless jobs.

---

## 12. Security and operational requirements

- Local-only bridge: named pipe / Unix socket with current-user access only; no TCP.
- Headless API server socket in a private runtime directory.
- Never log `KICAD_API_TOKEN` or full envelopes; redact by default.
- Treat all LLM-supplied values, paths, labels, and net names as untrusted.
- Output paths restricted to the project directory unless explicitly allowed.
- No shell: fixed executables and argument arrays only.
- Size and count limits on all inputs.
- Operation log: correlation ID, tool, target document, changed objects, result state, data origin; no tokens or unnecessary design content.
- `read_only` mode rejects all mutation tools.
- Explicit apply step required; destructive operations and overwrites out of scope for the MVP.
- IPC trace logs (`KICAD_ENABLE_WXTRACE=1`, `WXTRACE=KICAD_API`, `EnableAPILogging=1`) contain design data; enable only while debugging.

---

## 13. Build and installation workflow

### Prerequisites
- KiCad nightly (pinned build, §2.3) with API server enabled; KiCad 10.0.7 with API server enabled for Tier 2 IPC probes.
- Stable Rust; `protoc` (for `prost-build`). No NNG library or CMake is needed (native transport, §5.2).
- An MCP host that launches a stdio server (Claude Code / Claude Desktop / VS Code).

### Install selection
Configuration file lists KiCad installs explicitly. Defaults on this machine:

```toml
[[kicad.install]]
name = "nightly"
cli  = 'D:\Program Files\KiCad\10.99\bin\kicad-cli.exe'
tier = 1

[[kicad.install]]
name = "stable"
cli  = 'C:\Program Files\KiCad\10.0\bin\kicad-cli.exe'
tier = 2
```

If no file exists: `KICAD_HOME`, then standard install directories. `session_status` reports which install was selected and why.

### Local test loop
1. `cargo build`.
2. `scripts\install-plugin.ps1` (KiCad 11 plugin directory only).
3. Headless path: run the MCP server, `session_open` with provider `headless` on a disposable project.
4. GUI path: open the disposable project in the nightly, press **KiCad AI: connect**, `session_open` with provider `gui`.
5. Tier 2: close KiCad 10, `session_open` with provider `file`.
6. Run `cargo test`, then live tests with `KICAD_LIVE=1`.

### Logging
- MCP protocol on stdout only; logs to stderr and a private rotating file.
- Separate MCP, bridge, and headless-server logs, joined by correlation ID.

---

## 14. Delivery plan

Sequenced by risk. One developer.

### Phase 0 — Compatibility spike (both versions)
**Deliverable:** `scripts\probe-kicad.ps1` plus a Rust probe binary, and `docs/kicad-api-compatibility.md` filled with observed results.

Probe matrix:

| Probe | Nightly headless | Nightly GUI | 10.0.7 GUI |
|---|---|---|---|
| Endpoint addressing on Windows (named pipe naming with `--socket`; default socket) | ✓ | ✓ | ✓ |
| NNG handshake, `GetVersion`, `Ping` | ✓ | ✓ | ✓ |
| Token: empty token accepted; pinned token; mismatch error | ✓ | ✓ | ✓ |
| `GetOpenDocuments` (schematic, PCB) | ✓ | ✓ | ✓ |
| Open a project/schematic in headless | ✓ | — | — |
| `GetSchematicHierarchy`, `GetSchematicNetlist` | ✓ | ✓ | expect `AS_UNHANDLED` |
| `PlaceSymbolFromLibrary` (incl. library loading in headless) | ✓ | ✓ | expect unsupported |
| `CreateItems` wire + local label; `BeginCommit`/`EndCommit` | ✓ | ✓ | expect unsupported |
| `SaveDocument`; `RevertDocument` | ✓ | ✓ | ✓ (record whether eeschema 10 answers) |
| Schematic job (ERC/export) over IPC | ✓ | ✓ | — |
| Generated v11 protos decode KiCad 10 replies | — | — | ✓ |
| Executable plugin action lifetime (schematic scope) | — | ✓ | — |

Exit criteria: every row recorded as supported / unsupported / failing, with captured request/response logs; proto commit and checksums recorded; license confirmed as GPL-3.0-or-later. **Do not proceed** to Phase 2 until the headless column passes.

**Status (24 Oct 2026): Phase 0 complete**, except the executable-plugin lifetime check, which moves to Phase 4 (the headless path does not depend on it).

- **Nightly headless:** two-resistor demo end to end (PowerShell probe and Rust client with the native NNG transport); full document lifecycle (`CreateDocument`, modified state, `SaveDocument`, `RevertDocument`, `CloseDocument`); the saved copy's `kicad-cli` netlist matches the intent.
- **Nightly GUI:** same demo inside one commit, which a single undo removes; token mismatch refused; IPC netlist and SVG export jobs succeed. No ERC job exists over IPC.
- **KiCad 10.0.7 GUI:** KiCad 11 protos decode its replies; all schematic commands and generic item/selection/save commands are unhandled; open documents are reported by filename only. Confirms the file + CLI fallback design.
- **Design changes recorded:** native transport (§5.2); library loading, document lifecycle, readiness polling, non-atomic headless applies, commit ownership, and single GUI endpoint on Windows (§3.1); ERC via CLI on both versions (§3.3).

Details in `docs/kicad-api-compatibility.md`.

### Phase 1 — Foundations
`kicad-domain` (IR, plans, result states, capabilities), `kicad-ipc` (envelope, status mapping, serialized client), `kicad-ipc-mock`, `kicad-cli-runner` (install discovery, ERC JSON, netlist). Headless provider lifecycle. MCP skeleton with `session_status` and `session_open`.

Exit: unit and protocol tests green on Windows CI; `session_status` from a real MCP host against the nightly headless server.

### Phase 2 — KiCad 11 read path
`sch_list_symbols`, `sch_get_symbol_pins`, `sch_get_netlist`, `session_list_documents` on IPC. Results agree with KiCad-authored fixtures.

### Phase 3 — KiCad 11 mutation path (headless)
Plan/apply, symbol placement, fields, wires, labels, one commit per apply, read-back, save, ERC, netlist export, `sch_render`. Two-resistor demo end-to-end.

Exit: all Tier 1 headless acceptance criteria.

### Phase 4 — KiCad 11 GUI plugin provider
`kicad-ai-plugin` as a registrar (revision 3: no bridge protocol): the toolbar button records the editor's socket and token, and the MCP server connects to it. Same demo live in the Schematic Editor with undo.

Exit: all Tier 1 acceptance criteria.

### Phase 5 — Readable schematics (layout engine)
Generated schematics must read like a person drew them, not only be electrically correct (owner review of a generated 3.3 V regulator, 24 Oct 2026). The netlist verification after every apply stays unchanged, so layout can never alter the circuit.

- Drawing conventions: power and ground nets as power symbols, oriented by net type; short wire stubs leading away from pins; labels oriented to read correctly; text clear of symbol bodies.
- Roles from the netlist: classify nets (power, ground, signal) and parts (series parts in the main path, shunt parts such as decoupling capacitors, sources such as connectors).
- Placement by signal flow: the main path left to right on a baseline; shunt parts grouped below the net they serve; 2-pin parts rotated and oriented (diode and LED polarity follows the flow).
- Wires: orthogonal routes between nearby pins that avoid symbol bodies, with junctions where wires meet; labels only for long or crossing connections.
- Optional layout hints in the circuit spec; editing tools (move, rotate, wire, delete) so a layout can be refined after creation (deferred; see tasks 5.9).
- Acceptance: golden circuits (3.3 V regulator first) pass automated readability checks (no overlapping parts or text, power as symbols, local nets drawn as wires) and a rendered review.

### Phase 6 — KiCad 10 fallback
**Done.** `kicad-sexpr` (parser with spans, symbol library and schematic readers, a writer that edits only the text a plan names; deletes and moves were added in Phase 8); the `file` provider (`FileSession`, behind `AnySession`) with guarded mutation — content-hash and lock-file checks, a `kicad-cli`-verified copy, atomic replace, a kept backup; the Phase 5 layout engine unchanged on file-derived symbol geometry; ERC, netlist, and export on the 10.0 CLI. The planned direct-socket check for open documents was replaced by the lock-file check (KiCad 10's API is too limited to add value). Backend-contract tests run the circuit on both backends and compare the CLI's netlists. Known limits: root sheet only, global library table not read, no PNG render with only KiCad 10 installed.

Exit: all Tier 2 acceptance criteria.

### Phase 7 — Hardening and packaging
**Done**, except the re-pin. A rotating log file for the MCP server; a failure-injection suite (`kicad-fault-tests`: a fake `kicad-cli` that crashes, hangs, lies, or races, and misbehaving API servers) with a source-level guard that no log call mentions a token; the observed capability matrix ([capability-matrix.md](capability-matrix.md)); release automation that tags `v<Cargo version>` and publishes a Windows zip, a PCM package, a Linux archive, and checksums ([releasing.md](releasing.md)); a PowerShell installer that sets up the server, the plugin, and the assistant entry, with a sandbox smoke test in CI; and the architecture documentation brought up to date ([architecture.md](architecture.md)). Re-pinning to KiCad 11.0 waits for its release: re-run `scripts/probe-kicad.ps1` and the live tests against it, update docs/kicad-api-compatibility.md, and bump the supported format range in `kicad-sexpr`.

### Phase 8 — Review and tidy existing schematics
**Done.** The whole sheet (parts, wires, labels, junctions, notes) is read from a live KiCad or a file; `sch_review` reports what is untidy; `sch_tidy_plan` re-lays out an existing circuit with the Phase 5 engine as moves, removals, and additions, applied by `sch_apply_plan` with the same verification (identical nets, no new ERC errors) on the editor, headless, and file providers; `sch_set_field` changes a value or footprint. Parts keep their identity and fields. Refused: buses, global or hierarchical labels, sheets, no-connect flags. Completed in v0.3.0: `sch_describe_layout`, `sch_arrange_plan` (move, turn, mirror, delete named parts; wires stay and break), `sch_reconnect_plan` (a router redraws every connection around the parts where they are, checked against the remembered circuit), and layout hints (`keep`, `place`, `near`) on `sch_tidy_plan`. Details and decisions: docs/tasks.md, Phase 8.

**Phase 9 (v0.4.0).** Text-aware checks (`TEXT_OVERLAP`) from estimated text boxes, `sch_declutter_plan`, `sch_label_plan`, `sch_draw_plan` (shapes, notes, titled boxes), `sch_edit_items_plan`, ids for every item in `sch_describe_layout`, and read-back of every written position, turn, and text move. Details: docs/tasks.md, Phase 9.

### Later
PCB tools and BOM/part tooling are planned in [pcb-and-bom-plan.md](pcb-and-bom-plan.md) (Phases 10 to 13). Also later: hierarchical sheets, multi-unit symbols, design-review checks, simulation hand-off, MCP prompts/resources and a bundled skill with KiCad conventions, on-demand tool groups, enclosure integration (§16.1).

---

## 15. Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Nightly API changes before 11.0 | Rework, broken probes | Pin build + proto commit; probe suite re-run on every re-pin; adapter isolation |
| KiCad 11 release slips | Tier 1 users must run nightly | Tier 2 fallback provides read/ERC/netlist and closed-file mutation on 10 |
| Headless API server endpoint/lifecycle differs from docs | Blocks first IPC path | Phase 0 first; plugin provider as alternative path |
| Native NNG transport diverges from KiCad's NNG behavior | IPC failures | Wire format verified in Phase 0; transport tests against a live server and the mock; `Transport` trait allows swapping in the C library |
| Headless edits are not atomic (no commit support) | Half-applied plan | Recorded apply with cleanup on failure; `partial` result state; mandatory read-back |
| Plugin process lifetime doesn't fit a persistent bridge | Bridge exits early | Launcher + managed child; headless path doesn't depend on it |
| File backend corrupts or upgrades a file | Data loss, unreadable in KiCad 10 | Lossless CST, round-trip fixtures, version preservation, atomic writes, CLI validation after write |
| Same document mutated via two paths (GUI + file/headless) | Lost edits on next GUI save | Lock-file and open-document checks; one provider per document |
| Connected-looking geometry differs from netlist | Wrong circuit | Grid snap; IPC or computed pin positions; netlist read-back |
| Retry duplicates objects | Corrupt schematic | Idempotency keys; read-back reconciliation |
| User switches document between plan and apply | Wrong target | Explicit targets; revision-bound plans |
| Too many low-level tools | Poor agent behavior | High-level batches; prefixed tool groups |
| Logs leak token or design IP | Security incident | Redaction, private paths, rotation |
| Modal GUI state blocks IPC | Timeouts | Deadlines, typed errors, no modal UI in bridge |
| OneDrive-redirected documents folder | Sync interference with plugin binaries/locks | Resolve path at runtime; document; allow plugin install path override |

---

## 16. Definition of done for the first release

From a clean checkout on Windows 11:

1. Build with locked dependencies.
2. **KiCad 11 headless:** MCP host launches `kicad-ai-mcp`; agent creates the two-resistor demo through plan/apply; save, netlist, ERC; netlist matches the fixture; file reopens in the nightly GUI.
3. **KiCad 11 GUI:** install the plugin, open the project, press **KiCad AI: connect**, repeat the demo live; one undo step reverts it.
4. **KiCad 10 fallback:** with KiCad 10.0.7, the same agent request produces a correct schematic via the file backend on a closed project, and the result opens in KiCad 10.0.7.
5. Close KiCad mid-session: tools report a clean disconnected error; reconnect and repeat.
6. `session_status` truthfully reports provider, version, and capability matrix in every case.

Do not market the result as an autonomous electrical engineer until electrical validation and simulation exist.

### 16.1 Suite roadmap note (enclosure design)
Keep `kicad-domain` KiCad-specific. When the mechanical/enclosure tools start, define a neutral exchange model (board outline, mounting holes, keep-outs, connector positions and orientations, component heights, STEP via `kicad-cli pcb export step`) in its own crate, consumed by both the KiCad and enclosure tool servers.

---

## Appendix A: Stable names

- Workspace: `thingsvu-tools`
- MCP binary: `kicad-ai-mcp`
- Plugin binary: `kicad-ai-plugin`
- Crates: `kicad-domain`, `kicad-ipc`, `kicad-sexpr`, `kicad-cli-runner`, `kicad-session`, `kicad-ipc-mock`, `kicad-fault-tests`
- First composite tool: `sch_create_demo_circuit`
- First fixture: `two_resistor_series_chain`

Tool and protocol names are versioned once published.

## Appendix B: Source checklist

1. KiCad IPC API — <https://dev-docs.kicad.org/en/apis-and-binding/ipc-api/>
2. Add-on developer guide — <https://dev-docs.kicad.org/en/apis-and-binding/ipc-api/for-addon-developers/>
3. Schematic API reference (KiCad 11) — <https://docs.kicad.org/kicad-python/schematic.html>
4. KiCad protos at the pinned commit — <https://gitlab.com/kicad/code/kicad/-/tree/d033635628/api/proto>
5. Plugin schema — <https://go.kicad.org/api/schemas/v1>
6. kicad-python source — <https://gitlab.com/kicad/code/kicad-python>
7. Rust MCP SDK — <https://github.com/modelcontextprotocol/rust-sdk>

---

**End of document.**
