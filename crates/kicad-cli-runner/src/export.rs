// SPDX-License-Identifier: GPL-3.0-or-later
//! `kicad-cli sch export png`: renders a schematic to PNG files (one per sheet).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::install::KicadInstall;
use crate::run::{check, run, CliError};

/// Renders `schematic` into `output_dir` (which must exist) and returns the PNG files written.
/// Needs a KiCad 11-series CLI; KiCad 10's `kicad-cli` has no PNG export.
pub async fn export_png(
    install: &KicadInstall,
    schematic: &Path,
    output_dir: &Path,
    dpi: u32,
    timeout: Duration,
) -> Result<Vec<PathBuf>, CliError> {
    let args: Vec<OsString> = vec![
        "sch".into(),
        "export".into(),
        "png".into(),
        "--dpi".into(),
        dpi.to_string().into(),
        "--output".into(),
        output_dir.into(),
        schematic.into(),
    ];
    check(run(&install.cli, &args, timeout).await?, schematic)?;
    let mut pngs: Vec<PathBuf> = std::fs::read_dir(output_dir)
        .map_err(|e| CliError::BadReport {
            path: output_dir.to_owned(),
            detail: e.to_string(),
        })?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("png"))
        .collect();
    pngs.sort();
    if pngs.is_empty() {
        return Err(CliError::BadReport {
            path: output_dir.to_owned(),
            detail: "no PNG was written".into(),
        });
    }
    Ok(pngs)
}
