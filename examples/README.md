# Examples

## Connecting an MCP host

Build the server once:

```powershell
cargo build --release -p kicad-ai-mcp
```

**Claude Code** (run in the repository folder):

```powershell
claude mcp add kicad-ai -- D:\projects\thingsvu-tools\target\release\kicad-ai-mcp.exe
```

**Claude Desktop and other hosts using `mcpServers` JSON:** merge [`mcp-client-config.example.json`](mcp-client-config.example.json) into the host's configuration, adjusting the path.

Then ask the assistant to call `session_status`. It reports the KiCad installs it found and what a session can do. To try a session, ask it to open a new schematic, for example:

> Use session_open with create: true on `C:\temp\try\first.kicad_sch`, show me the capabilities, then close it.

Options:
- `--kicad-cli <path>` prefers a specific `kicad-cli` over automatic discovery (repeatable).
- The `RUST_LOG` environment variable sets the log level; logs go to stderr.

## Trying the schematic tools

With a session open on a new schematic, ask for example:

> Use sch_create_demo_circuit on my session (plan only) and show me the plan. If it looks right, apply it with confirm: true and a new idempotency key, then run sch_run_erc and show me sch_render.

Or describe your own circuit as components and nets and ask the assistant to call `sch_plan_changes`, review the plan, then `sch_apply_plan`. Save with `sch_save`.

Start the server with `--read-only` to allow reading, planning, ERC, and rendering, while refusing every change to a schematic.
