# Capability matrix

What each kind of session can do, and which backend serves it. This is what `session_status` reports in `capabilities` for a live session; the table is what has been observed and tested. Backends: **ipc** is KiCad's API, **kicad_cli** is `kicad-cli`, **project_file** is the schematic file itself.

| Capability | Headless (KiCad 11 / 10.99) | GUI (KiCad 11 editor, via the plugin) | File (KiCad 10 or 11, no KiCad running) |
|---|---|---|---|
| `session.status` | ipc | ipc | project_file |
| `session.open_documents` | ipc | ipc | project_file (the session's one file) |
| `session.document_lifecycle` | ipc: create, open, save, revert, close | unavailable: the editor owns the document | unavailable: no editor document |
| `session.commits` | ipc, when the document was opened by the session (always the case here) | ipc: each commit is one undo step, and an apply is two | unavailable: changes are verified in a copy, then the file is replaced in one step |
| `sch.read_hierarchy` | ipc | ipc | unavailable: only the root schematic file is read |
| `sch.read_symbols` | ipc | ipc | project_file |
| `sch.netlist` | ipc | ipc | kicad_cli: the netlister omits power symbols and flags |
| `sch.place_symbol` | ipc | ipc | project_file: symbols from the file, the project's `sym-lib-table`, or the install's libraries |
| `sch.edit_items` | ipc | ipc | project_file |
| `sch.save` | ipc | ipc | project_file: every applied plan is written at once |
| `sch.erc` | kicad_cli, on a snapshot of the in-memory document | kicad_cli, on a snapshot | kicad_cli, on a copy of the file |
| `sch.render` | kicad_cli (PNG) | kicad_cli (PNG) | kicad_cli (PNG): needs a KiCad 11 / 10.99 install, because the 10.0 CLI cannot export PNG |
| `sch.export_netlist` | kicad_cli | kicad_cli | kicad_cli |

## Notes

- **A capability is probed, not assumed.** The API providers send a harmless request per command family when a session opens; an answer of "unhandled" or "unimplemented" turns the capability off, with the reason in `note`. The file provider's set is fixed by design, with the CLI-dependent entries turned off when no suitable `kicad-cli` is installed.
- **KiCad 10's API is not used.** KiCad 10.0.7 answers only version, open-document names, and commits; it has no schematic commands (docs/kicad-api-compatibility.md). A KiCad 10 user works through the file provider. There is no provider that attaches to a running KiCad 10 editor.
- **Which provider for which job:** the headless provider generates and checks designs without a GUI; the GUI provider co-edits the open editor with normal undo; the file provider needs no KiCad running and works with KiCad 10.
- **One provider per document.** A document open in an editor is never edited through the headless or file provider (lock-file check), and the GUI provider is the only one that edits it.
- **Version-specific `kicad-cli` differences** (PNG export, `sch diff`, `api-server`) are in docs/kicad-api-compatibility.md.
