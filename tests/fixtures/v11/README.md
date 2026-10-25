# KiCad 11 fixtures

Files written by KiCad itself (10.99 nightly `10.99.0-5896-gd033635628`, schematic format `20261015`). Do not edit them by hand; regenerate them with KiCad instead. KiCad 10 cannot open these files.

| Fixture | Source |
|---|---|
| `empty-schematic/` | New project created in the 10.99 GUI |
| `two-resistor-series-chain/` | `empty-schematic` after the Rust probe `demo` scenario on the headless server. Values were set with `UpdateItems` without the embedded library definition and with field mask `value_field`. One library symbol (`Device:R`); R1 and R2 are `10k` |

Expected connectivity for `two-resistor-series-chain` (from `kicad-cli sch export netlist`, matching `docs/examples/two-resistor-series-chain.json`):

| Net | Nodes |
|---|---|
| `/INPUT` | R1.1 |
| `/MID` | R1.2, R2.1 |
| `/OUTPUT` | R2.2 |

ERC (`kicad-cli sch erc --severity-all`): 2 warnings, `isolated_pin_label` on INPUT and OUTPUT. This is expected, because those nets have one pin each.

History: earlier versions of this fixture were wrong. The first headless one duplicated the library symbol (`R_1`) because whole symbols were sent back in one batch. A GUI-saved one kept Value `R` although `UpdateItems` replied `10k`. Replaced on 24 Oct 2026; see `docs/tasks.md` (tasks 1.4 and 2.6).

| `regulator-with-notes/` | The 3.3 V regulator from `docs/examples/regulator-3v3.json`, built with these tools in the 10.99 GUI and then edited by an AI assistant (a title and a note added, labels and flags moved). A realistic starting point for the tidy tests: 13 parts, 11 power symbols, wires, junctions, two text notes. Do not edit by hand |
