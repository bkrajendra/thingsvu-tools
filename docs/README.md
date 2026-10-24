# KiCad AI MCP design-plan archive

This archive contains the architecture, technical specification, milestones, tool contracts, and validation plan for a Rust MCP server plus executable IPC plugin. KiCad 11 is the primary target, developed against a pinned 10.99 nightly; KiCad 10.0.x is a supported fallback.

Files:
- `kicad-ai-mcp-design-plan.md` — main design document (revision 2).
- `kicad-api-compatibility.md` — pinned KiCad builds, proto commit, CLI differences, and Phase 0 probe results.
- `tasks.md` — phase-by-phase task list with decisions to review.
- The KiCad 11 plugin manifest lives in `plugin-package/plugin.json.template`; `scripts/install-plugin.ps1` installs it.
- `examples/two-resistor-series-chain.json` — circuit intent for the first integration test.

The installer fills in the plugin executable path; the template is valid against the plugin schema shipped with the pinned KiCad build.
