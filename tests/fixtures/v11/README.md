# KiCad 11 fixtures

Files written by KiCad itself (10.99 nightly `10.99.0-5896-gd033635628`, schematic format `20261015`). Do not edit them by hand; regenerate them with KiCad instead. KiCad 10 cannot open these files.

| Fixture | Source |
|---|---|
| `empty-schematic/` | New project created in the 10.99 GUI |
| `two-resistor-series-chain/` | `empty-schematic` after `scripts/probe-kicad.ps1 -Scenario demo`, saved with `SaveCopyOfDocument` through the headless API server |

Expected connectivity for `two-resistor-series-chain` (from `kicad-cli sch export netlist`, matching `docs/examples/two-resistor-series-chain.json`):

| Net | Nodes |
|---|---|
| `/INPUT` | R1.1 |
| `/MID` | R1.2, R2.1 |
| `/OUTPUT` | R2.2 |

ERC (`kicad-cli sch erc --severity-all`): 2 warnings, `isolated_pin_label` on INPUT and OUTPUT. This is expected, because those nets have one pin each.

Known artifact: the probe set values by sending each whole symbol back through `UpdateItems`, so `two-resistor-series-chain` also contains a duplicate library symbol `R_1`, and its netlist reports `libsource` `:R_1`. Regenerate the fixture once value updates send only the changed field (tracked in `docs/tasks.md`, task 1.4).
