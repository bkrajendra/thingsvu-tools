# KiCad AI MCP design-plan archive

This archive contains the architecture, technical specification, milestones, tool contracts, and validation plan for a Rust MCP server plus executable IPC plugin. KiCad 11 is the primary target, developed against a pinned 10.99 nightly; KiCad 10.0.x is a supported fallback.

Files:
- `kicad-ai-mcp-design-plan.md` — main design document (revision 2).
- `kicad-api-compatibility.md` — pinned KiCad builds, proto commit, CLI differences, and Phase 0 probe results.
- `plugin.json.example` — example manifest for the KiCad 11 executable plugin (schematic and PCB scopes). KiCad 10 does not support the schematic scope.
- `examples/two-resistor-series-chain.json` — circuit intent for the first integration test.

Before installing the plugin, replace the executable path in `plugin.json.example` and validate it against the plugin schema shipped with the pinned KiCad build.
