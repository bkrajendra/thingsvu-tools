# Documentation

How thingsvu-tools is built, what it can do, and how it is released. For installing and using it, start with the [README](../README.md).

| Document | What it is |
|---|---|
| [architecture.md](architecture.md) | **Start here.** The current component and crate diagrams, the editor registration flow, how plans are made and applied, the release pipeline, and what changed from the original plan |
| [capability-matrix.md](capability-matrix.md) | What each kind of session (`headless`, `gui`, `file`) can do, and which backend serves it |
| [kicad-api-compatibility.md](kicad-api-compatibility.md) | Pinned KiCad builds, the proto commit, `kicad-cli` differences, and everything observed while probing KiCad 10 and 11 |
| [releasing.md](releasing.md) | How a release is made (the Cargo version drives the tag) and what is published |
| [pcb-and-bom-plan.md](pcb-and-bom-plan.md) | The plan for parts, footprints, BOM, and PCB tools (Phases 10 to 13): goals, what KiCad allows, decisions for review, and risks |
| [tasks.md](tasks.md) | Phase-by-phase task list, with the decisions and workarounds that need the project owner's review |
| [kicad-ai-mcp-design-plan.md](kicad-ai-mcp-design-plan.md) | The design plan: reasoning, requirements, tool contracts, test plan, and risks. Written before the code; where it differs from [architecture.md](architecture.md), the architecture page is right |
| [examples/](examples/) | Circuit descriptions used as test fixtures: a two-resistor chain, a 3.3 V regulator, an NE555 blinker |

Other files that matter:

- `plugin-package/plugin.json` is the KiCad 11 plugin manifest. It uses a plugin-relative entrypoint, because KiCad skips absolute ones, and is valid against the plugin schema shipped with the pinned KiCad build.
- `scripts/` holds the installer and uninstaller (shipped in the release zip), the packaging and installer test scripts, and the developer scripts.
