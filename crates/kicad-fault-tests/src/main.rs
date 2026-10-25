// SPDX-License-Identifier: GPL-3.0-or-later
//! A stand-in for `kicad-cli` that fails on purpose. It handles only
//! `sch export netlist … --output <file> <schematic>`, and how it behaves comes from a file named
//! `.fake-mode` in the schematic's folder (so tests running side by side do not share settings):
//!
//! - `crash`: exits with an error, as `kicad-cli` does for a file it cannot load
//! - `hang`: never finishes
//! - `garbage`: writes something that is not a netlist
//! - `edit-original`: writes a valid empty netlist, but first changes the file named in
//!   `.fake-target` (as an editor saving the file would)
//! - anything else: writes a valid, empty netlist

use std::path::{Path, PathBuf};

const EMPTY_NETLIST: &str =
    "(export (version \"E\")\n  (design (source \"fake\") (tool \"Eeschema fake\"))\n  (components)\n  (nets))\n";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let schematic = args.last().map(PathBuf::from).unwrap_or_default();
    let dir = schematic.parent().unwrap_or(Path::new("."));
    let mode = std::fs::read_to_string(dir.join(".fake-mode")).unwrap_or_default();
    let output = args
        .iter()
        .position(|a| a == "--output")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    match mode.trim() {
        "crash" => {
            eprintln!("Failed to load schematic");
            std::process::exit(3);
        }
        "hang" => std::thread::sleep(std::time::Duration::from_secs(120)),
        "garbage" => {
            if let Some(output) = output {
                std::fs::write(output, "this is not a netlist").unwrap();
            }
        }
        "edit-original" => {
            if let Ok(target) = std::fs::read_to_string(dir.join(".fake-target")) {
                let target = PathBuf::from(target.trim());
                let mut text = std::fs::read_to_string(&target).unwrap_or_default();
                text.push_str("\n; edited while the plan was being verified\n");
                std::fs::write(&target, text).unwrap();
            }
            if let Some(output) = output {
                std::fs::write(output, EMPTY_NETLIST).unwrap();
            }
        }
        _ => {
            if let Some(output) = output {
                std::fs::write(output, EMPTY_NETLIST).unwrap();
            }
        }
    }
}
