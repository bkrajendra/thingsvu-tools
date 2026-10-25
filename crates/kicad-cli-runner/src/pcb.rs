// SPDX-License-Identifier: GPL-3.0-or-later
//! `kicad-cli pcb drc` (the JSON report parsed) and `kicad-cli pcb render` (a PNG of the board).

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::install::KicadInstall;
use crate::run::{check, run, CliError};

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DrcPos {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DrcItem {
    pub description: String,
    pub pos: Option<DrcPos>,
}

/// One violation, or one missing connection (which has no type or severity of its own).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DrcViolation {
    #[serde(default, rename = "type")]
    pub kind: String,
    pub description: String,
    #[serde(default)]
    pub severity: String,
    #[serde(default)]
    pub items: Vec<DrcItem>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DrcReport {
    pub kicad_version: String,
    #[serde(default)]
    pub violations: Vec<DrcViolation>,
    #[serde(default)]
    pub unconnected_items: Vec<DrcViolation>,
    /// Differences from the schematic, when the run asked for them.
    #[serde(default)]
    pub schematic_parity: Vec<DrcViolation>,
}

impl DrcReport {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn count(&self, severity: &str) -> usize {
        self.violations.iter().filter(|v| v.severity == severity).count()
    }
}

/// Runs DRC on `board`, writing the JSON report to `report_path`. Violations do not make this
/// fail; only a failure to run is an error. `parity` also compares the board with its schematic.
pub async fn run_drc(
    install: &KicadInstall,
    board: &Path,
    report_path: &Path,
    parity: bool,
    timeout: Duration,
) -> Result<DrcReport, CliError> {
    let mut args: Vec<OsString> = vec![
        "pcb".into(),
        "drc".into(),
        "--format".into(),
        "json".into(),
        "--severity-all".into(),
        "--units".into(),
        "mm".into(),
    ];
    if parity {
        args.push("--schematic-parity".into());
    }
    args.push("--output".into());
    args.push(report_path.into());
    args.push(board.into());
    check(run(&install.cli, &args, timeout).await?, board)?;
    let json = std::fs::read_to_string(report_path).map_err(|e| CliError::BadReport {
        path: report_path.to_owned(),
        detail: e.to_string(),
    })?;
    DrcReport::parse(&json).map_err(|e| CliError::BadReport {
        path: report_path.to_owned(),
        detail: e.to_string(),
    })
}

/// Renders the board in 3D to a PNG at `output`. Needs a KiCad 11-series CLI. `side` is one of
/// `top`, `bottom`, `left`, `right`, `front`, `back`.
/// Writes `board` as a Specctra DSN file for an autorouter (KiCad 10.99 and later).
pub async fn export_specctra(
    install: &KicadInstall,
    board: &Path,
    output: &Path,
    timeout: Duration,
) -> Result<(), CliError> {
    let args: Vec<OsString> = vec![
        "pcb".into(),
        "export".into(),
        "specctra".into(),
        "--output".into(),
        output.into(),
        board.into(),
    ];
    check(run(&install.cli, &args, timeout).await?, board)?;
    if !output.is_file() {
        return Err(CliError::BadReport {
            path: output.to_owned(),
            detail: "no DSN file was written".into(),
        });
    }
    Ok(())
}

pub async fn render_png(
    install: &KicadInstall,
    board: &Path,
    output: &Path,
    side: &str,
    width: u32,
    height: u32,
    timeout: Duration,
) -> Result<(), CliError> {
    let args: Vec<OsString> = vec![
        "pcb".into(),
        "render".into(),
        "--side".into(),
        side.into(),
        "--width".into(),
        width.to_string().into(),
        "--height".into(),
        height.to_string().into(),
        "--background".into(),
        "opaque".into(),
        "--output".into(),
        output.into(),
        board.into(),
    ];
    check(run(&install.cli, &args, timeout).await?, board)?;
    if !output.is_file() {
        return Err(CliError::BadReport {
            path: output.to_owned(),
            detail: "no image was written".into(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drc_report_is_parsed() {
        let report = DrcReport::parse(include_str!("../tests/data/drc-regulator-board.json")).unwrap();
        assert_eq!(report.kicad_version, "10.99.0");
        assert_eq!(report.violations.len(), 9);
        assert_eq!(report.count("error"), 1);
        assert_eq!(report.count("warning"), 8);
        assert_eq!(
            report.violations.iter().find(|v| v.severity == "error").unwrap().kind,
            "invalid_outline"
        );
        assert_eq!(report.unconnected_items.len(), 18);
        assert!(report.unconnected_items[0].items[0].description.contains("[/VBUS]"));
    }
}
