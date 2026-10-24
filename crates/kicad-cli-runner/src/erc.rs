// SPDX-License-Identifier: GPL-3.0-or-later
//! `kicad-cli sch erc` with the JSON report (`https://schemas.kicad.org/erc.v1.json`) parsed.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::install::KicadInstall;
use crate::run::{check, run, CliError};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErcReport {
    #[serde(default)]
    pub kicad_version: String,
    #[serde(default)]
    pub coordinate_units: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub sheets: Vec<ErcSheet>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErcSheet {
    pub path: String,
    #[serde(default)]
    pub uuid_path: String,
    #[serde(default)]
    pub violations: Vec<ErcViolation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErcViolation {
    /// `error`, `warning`, or `exclusion`.
    pub severity: String,
    /// Stable check key, e.g. `isolated_pin_label`.
    #[serde(rename = "type")]
    pub kind: String,
    pub description: String,
    #[serde(default)]
    pub items: Vec<ErcItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErcItem {
    pub description: String,
    #[serde(default)]
    pub pos: Option<ErcPos>,
    #[serde(default)]
    pub uuid: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ErcPos {
    pub x: f64,
    pub y: f64,
}

impl ErcReport {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn violations(&self) -> impl Iterator<Item = (&ErcSheet, &ErcViolation)> {
        self.sheets
            .iter()
            .flat_map(|s| s.violations.iter().map(move |v| (s, v)))
    }

    pub fn count(&self, severity: &str) -> usize {
        self.violations().filter(|(_, v)| v.severity == severity).count()
    }
}

/// Runs ERC on `schematic`, writing the JSON report to `report_path`. Violations do not make this
/// fail: the report is returned and the caller decides. Only a failure to run is an error.
pub async fn run_erc(
    install: &KicadInstall,
    schematic: &Path,
    report_path: &Path,
    timeout: Duration,
) -> Result<ErcReport, CliError> {
    let args: Vec<OsString> = vec![
        "sch".into(),
        "erc".into(),
        "--format".into(),
        "json".into(),
        "--severity-all".into(),
        "--output".into(),
        report_path.into(),
        schematic.into(),
    ];
    check(run(&install.cli, &args, timeout).await?, schematic)?;
    let json = std::fs::read_to_string(report_path).map_err(|e| CliError::BadReport {
        path: report_path.to_owned(),
        detail: e.to_string(),
    })?;
    ErcReport::parse(&json).map_err(|e| CliError::BadReport {
        path: report_path.to_owned(),
        detail: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_report_from_kicad_10_99() {
        let report = ErcReport::parse(include_str!("../tests/data/erc-two-resistor.json")).unwrap();
        assert_eq!(report.kicad_version, "10.99.0");
        assert_eq!(report.count("warning"), 2);
        assert_eq!(report.count("error"), 0);
        let (sheet, first) = report.violations().next().unwrap();
        assert_eq!(sheet.path, "/");
        assert_eq!(first.kind, "isolated_pin_label");
        assert_eq!(first.items[0].pos, Some(ErcPos { x: 50.8, y: 46.99 }));
    }
}
