// SPDX-License-Identifier: GPL-3.0-or-later
//! Runs `kicad-cli` with fixed argument lists (never through a shell) and parses its reports.
//!
//! - [`install`]: find KiCad installs and their versions.
//! - [`erc`]: electrical rules check with the JSON report parsed.
//! - [`netlist`]: netlist export with the KiCad s-expression netlist parsed.
//! - [`pcb`]: board DRC with the JSON report parsed, and 3D renders.
//! - [`fab`]: manufacturing outputs (Gerbers, drill, positions, STEP, plots) and Gerber read-back.

pub mod erc;
pub mod export;
pub mod fab;
pub mod freerouting;
pub mod install;
pub mod netlist;
pub mod pcb;
mod run;
mod sexpr;

pub use install::{discover, InstallSource, KicadInstall, KicadVersion};
pub use run::{CliError, CliOutput};
