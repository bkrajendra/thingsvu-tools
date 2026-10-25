<h1 align="center">thingsvu-tools</h1>

<p align="center">
  <b>AI-assisted design tools for embedded products — schematic, PCB, and enclosure — built on KiCad.</b>
</p>

<p align="center">
  <a href="#project-status"><img alt="Status: pre-alpha" src="https://img.shields.io/badge/status-pre--alpha-orange"></a>
  <a href="LICENSE"><img alt="License: GPL-3.0-or-later" src="https://img.shields.io/badge/license-GPL--3.0--or--later-blue"></a>
  <img alt="KiCad 11" src="https://img.shields.io/badge/KiCad-11%20(first--class)-314cb0">
  <img alt="KiCad 10" src="https://img.shields.io/badge/KiCad-10%20(fallback)-6b7bc4">
  <img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-Rust-b7410e">
  <img alt="MCP" src="https://img.shields.io/badge/protocol-MCP-6e40c9">
  <a href="https://github.com/sponsors/bkrajendra"><img alt="Sponsor" src="https://img.shields.io/badge/sponsor-%E2%9D%A4-ea4aaa"></a>
</p>

---

## What is thingsvu-tools?

thingsvu-tools lets an AI assistant work alongside you in KiCad. You describe the circuit you want; the assistant plans the change, shows you exactly what it will do, and — once you approve — builds it in your schematic. Every result is checked against KiCad itself before it is reported back to you.

It connects to any assistant that speaks the [Model Context Protocol](https://modelcontextprotocol.io) (MCP), such as Claude Desktop, Claude Code, or VS Code.

## Why it's different

- **You stay in control.** Changes are proposed as a plan first. Nothing touches your design until you confirm it, and a stale plan is never applied to a document that has changed.
- **Verified, not assumed.** After every change, the tools read the design back from KiCad — symbols, values, and net connections — and run ERC. A check that couldn't run is reported as such, never as a pass.
- **Describe intent, not coordinates.** Say "R1 pin 2 and R2 pin 1 share net MID"; the tools take care of placement, wiring, and labels on the proper grid.
- **Built on KiCad's official API.** First-class support for KiCad 11's schematic API, with a graceful fallback on KiCad 10. You always know which features your KiCad version supports.
- **Works live or headless.** Co-edit in the open Schematic Editor with normal undo, or generate and verify designs without opening the GUI.
- **Local and private.** Everything runs on your machine. No cloud service, no open network ports, and your design files stay where they are.
- **Fast and dependable.** Written in Rust as a small set of native binaries — no Python runtime to manage.

## What's included

| Component | What it does |
|---|---|
| **kicad-ai-mcp** | The MCP server your AI assistant talks to |
| **kicad-ai-plugin** | A KiCad plugin that connects the assistant to your open editor |
| **kicad-domain** | The shared circuit model: plans, validation, and layout rules |

## Roadmap

- **Now:** schematic inspection and editing, readable generated schematics (signal flow left to right, power symbols, wires), ERC, netlist export, verified demo circuits, and the same on KiCad 10 through the schematic file
- **Next:** layout editing tools, PCB tools, hierarchical schematics, part and BOM assistance
- **Later:** 3D enclosure design driven from your PCB, so the whole embedded product comes together in one workflow

## Project status

thingsvu-tools is in early development and is not ready for production designs yet. The architecture and plan are in [`docs/`](docs/README.md).

AI-generated designs do not replace qualified engineering review. Always review and verify a design before manufacturing it.

## Supported KiCad versions

| KiCad | Support |
|---|---|
| 11 (currently nightly 10.99) | First-class: full schematic editing, live and headless |
| 10.0.x | Fallback through the schematic file: inspection, ERC, netlist, and editing of schematics no editor has open (each change is checked with `kicad-cli` before it replaces the file) |

## Sponsor

If thingsvu-tools is useful to you, please consider [sponsoring its development](https://github.com/sponsors/bkrajendra). Sponsorship helps keep the project independent and moving forward.

## License

thingsvu-tools is free software, licensed under the [GNU General Public License v3.0 or later](LICENSE). thingsvu-tools is an independent project and is not affiliated with or endorsed by the KiCad project.
