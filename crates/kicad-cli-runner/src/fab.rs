// SPDX-License-Identifier: GPL-3.0-or-later
//! Manufacturing outputs through `kicad-cli pcb export …` (Phase 13): Gerbers with their job file,
//! Excellon drill files, the position file, STEP, and PDF or SVG plots; and `kicad-cli gerber
//! info`, the reader of KiCad's Gerber viewer (KiCad 11-series CLI), to read each file back.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::install::KicadInstall;
use crate::run::{check, run, CliError};

/// The layers a board house needs besides copper, by KiCad's untranslated names.
pub const FAB_LAYERS: [&str; 7] = [
    "F.Paste",
    "B.Paste",
    "F.SilkS",
    "B.SilkS",
    "F.Mask",
    "B.Mask",
    "Edge.Cuts",
];

/// The Gerber layers for a board with these copper layers: copper first, then [`FAB_LAYERS`].
pub fn fab_layers(copper: &[String]) -> Vec<String> {
    copper
        .iter()
        .cloned()
        .chain(FAB_LAYERS.iter().map(|l| (*l).to_owned()))
        .collect()
}

/// The files in `dir` (not its folders), sorted.
fn files_in(dir: &Path) -> Result<Vec<PathBuf>, CliError> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| CliError::BadReport {
            path: dir.to_owned(),
            detail: e.to_string(),
        })?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    Ok(files)
}

fn written(files: Vec<PathBuf>, dir: &Path, what: &str) -> Result<Vec<PathBuf>, CliError> {
    if files.is_empty() {
        return Err(CliError::BadReport {
            path: dir.to_owned(),
            detail: format!("no {what} was written"),
        });
    }
    Ok(files)
}

/// Plots `layers` of `board` as Gerbers (X2, with a `.gbrjob` job file) into `output_dir`, which
/// must exist and be empty. Zones are refilled first. Returns the files written.
pub async fn export_gerbers(
    install: &KicadInstall,
    board: &Path,
    output_dir: &Path,
    layers: &[String],
    timeout: Duration,
) -> Result<Vec<PathBuf>, CliError> {
    let args: Vec<OsString> = vec![
        "pcb".into(),
        "export".into(),
        "gerbers".into(),
        "--layers".into(),
        layers.join(",").into(),
        "--check-zones".into(),
        "--output".into(),
        output_dir.into(),
        board.into(),
    ];
    check(run(&install.cli, &args, timeout).await?, board)?;
    written(files_in(output_dir)?, output_dir, "Gerber")
}

/// Writes Excellon drill files in millimetres into `output_dir` (which must exist and be empty),
/// plated and non-plated holes in separate files. Returns the files written.
pub async fn export_drill(
    install: &KicadInstall,
    board: &Path,
    output_dir: &Path,
    timeout: Duration,
) -> Result<Vec<PathBuf>, CliError> {
    let args: Vec<OsString> = vec![
        "pcb".into(),
        "export".into(),
        "drill".into(),
        "--format".into(),
        "excellon".into(),
        "--excellon-units".into(),
        "mm".into(),
        "--excellon-separate-th".into(),
        "--output".into(),
        output_dir.into(),
        board.into(),
    ];
    check(run(&install.cli, &args, timeout).await?, board)?;
    written(files_in(output_dir)?, output_dir, "drill file")
}

/// Writes the position (pick and place) file as CSV in millimetres, both sides.
pub async fn export_positions(
    install: &KicadInstall,
    board: &Path,
    output: &Path,
    exclude_dnp: bool,
    timeout: Duration,
) -> Result<(), CliError> {
    let mut args: Vec<OsString> = vec![
        "pcb".into(),
        "export".into(),
        "pos".into(),
        "--format".into(),
        "csv".into(),
        "--units".into(),
        "mm".into(),
        "--side".into(),
        "both".into(),
    ];
    if exclude_dnp {
        args.push("--exclude-dnp".into());
    }
    args.extend(["--output".into(), output.into(), board.into()]);
    check(run(&install.cli, &args, timeout).await?, board)?;
    exists(output, "position file")
}

/// Writes the board and its parts as STEP (STEP models used in place of VRML ones where both
/// exist). Returns what `kicad-cli` printed about missing models.
pub async fn export_step(
    install: &KicadInstall,
    board: &Path,
    output: &Path,
    timeout: Duration,
) -> Result<Vec<String>, CliError> {
    let args: Vec<OsString> = vec![
        "pcb".into(),
        "export".into(),
        "step".into(),
        "--force".into(),
        "--subst-models".into(),
        "--output".into(),
        output.into(),
        board.into(),
    ];
    let out = check(run(&install.cli, &args, timeout).await?, board)?;
    exists(output, "STEP file")?;
    Ok(format!("{}\n{}", out.stdout, out.stderr)
        .lines()
        .map(str::trim)
        .filter(|l| l.contains("Warning") || l.contains("not found") || l.contains("Could not"))
        .map(str::to_owned)
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlotFormat {
    /// One PDF with a page per layer.
    Pdf,
    /// One SVG per layer, the page cut to the board.
    Svg,
}

/// Plots `layers` for people to look at. A PDF is written to `output_dir/<name>.pdf`; SVGs one per
/// layer into `output_dir`. Returns the files written.
pub async fn export_plot(
    install: &KicadInstall,
    board: &Path,
    output_dir: &Path,
    format: PlotFormat,
    layers: &[String],
    timeout: Duration,
) -> Result<Vec<PathBuf>, CliError> {
    let stem = board
        .file_stem()
        .map(|s| s.to_owned())
        .unwrap_or_else(|| "board".into());
    let mut args: Vec<OsString> = vec!["pcb".into(), "export".into()];
    match format {
        PlotFormat::Pdf => {
            let mut file = stem;
            file.push(".pdf");
            args.extend([
                "pdf".into(),
                "--mode-multipage".into(),
                "--output".into(),
                output_dir.join(file).into(),
            ]);
        }
        PlotFormat::Svg => args.extend([
            "svg".into(),
            "--mode-multi".into(),
            "--exclude-drawing-sheet".into(),
            "--page-size-mode".into(),
            "2".into(),
            "--output".into(),
            output_dir.into(),
        ]),
    }
    args.extend([
        "--layers".into(),
        layers.join(",").into(),
        "--check-zones".into(),
        board.into(),
    ]);
    check(run(&install.cli, &args, timeout).await?, board)?;
    written(files_in(output_dir)?, output_dir, "plot")
}

fn exists(path: &Path, what: &str) -> Result<(), CliError> {
    if path.is_file() {
        Ok(())
    } else {
        Err(CliError::BadReport {
            path: path.to_owned(),
            detail: format!("no {what} was written"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct GerberBox {
    pub origin_x: f64,
    pub origin_y: f64,
    pub width: f64,
    pub height: f64,
}

/// A Gerber or Excellon file as KiCad's Gerber viewer reads it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct GerberInfo {
    /// `Gerber` or `Excellon Drill`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Shapes in a Gerber, holes in a drill file.
    pub item_count: usize,
    pub aperture_count: usize,
    pub bounding_box: GerberBox,
}

/// Reads `file` with `kicad-cli gerber info --strict`, which fails on any parse warning. Needs a
/// KiCad 11-series CLI.
pub async fn gerber_info(install: &KicadInstall, file: &Path, timeout: Duration) -> Result<GerberInfo, CliError> {
    let args: Vec<OsString> = vec![
        "gerber".into(),
        "info".into(),
        "--format".into(),
        "json".into(),
        "--units".into(),
        "mm".into(),
        "--strict".into(),
        file.into(),
    ];
    let out = check(run(&install.cli, &args, timeout).await?, file)?;
    serde_json::from_str(&out.stdout).map_err(|e| CliError::BadReport {
        path: file.to_owned(),
        detail: e.to_string(),
    })
}

/// One file named in a Gerber job file.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobFile {
    pub path: String,
    /// The X2 file function, e.g. `Copper,L1,Top`, `Legend,Bot`, `Profile,NP`.
    pub function: String,
}

/// What a `.gbrjob` file (JSON) says about the board.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GerberJob {
    pub copper_layers: Option<u32>,
    pub thickness_mm: Option<f64>,
    pub files: Vec<JobFile>,
}

impl GerberJob {
    pub fn parse(json: &str) -> Result<GerberJob, serde_json::Error> {
        let v: serde_json::Value = serde_json::from_str(json)?;
        let specs = &v["GeneralSpecs"];
        Ok(GerberJob {
            copper_layers: specs["LayerNumber"].as_u64().map(|n| n as u32),
            thickness_mm: specs["BoardThickness"].as_f64(),
            files: v["FilesAttributes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| {
                    Some(JobFile {
                        path: f["Path"].as_str()?.to_owned(),
                        function: f["FileFunction"].as_str().unwrap_or_default().to_owned(),
                    })
                })
                .collect(),
        })
    }

    /// The files that are copper layers.
    pub fn copper_files(&self) -> usize {
        self.files.iter().filter(|f| f.function.starts_with("Copper,")).count()
    }

    /// Whether the board outline (`Profile`) is among the files.
    pub fn has_profile(&self) -> bool {
        self.files.iter().any(|f| f.function.starts_with("Profile"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_file_is_read() {
        let job = GerberJob::parse(
            r#"{ "GeneralSpecs": { "LayerNumber": 2, "BoardThickness": 1.6 },
                 "FilesAttributes": [
                   { "Path": "b-F_Cu.gtl", "FileFunction": "Copper,L1,Top", "FilePolarity": "Positive" },
                   { "Path": "b-B_Cu.gbl", "FileFunction": "Copper,L2,Bot", "FilePolarity": "Positive" },
                   { "Path": "b-F_Silkscreen.gto", "FileFunction": "Legend,Top", "FilePolarity": "Positive" },
                   { "Path": "b-Edge_Cuts.gm1", "FileFunction": "Profile,NP", "FilePolarity": "Positive" } ] }"#,
        )
        .unwrap();
        assert_eq!((job.copper_layers, job.thickness_mm), (Some(2), Some(1.6)));
        assert_eq!(job.copper_files(), 2);
        assert!(job.has_profile());
        assert_eq!(job.files[2].function, "Legend,Top");
    }

    #[test]
    fn gerber_info_is_read() {
        let info: GerberInfo = serde_json::from_str(
            r#"{ "aperture_count": 1, "bounding_box": { "height": 3.54, "origin_x": 15.15, "origin_y": 1.29,
                 "width": 1.0 }, "filename": "b.drl", "item_count": 2, "type": "Excellon Drill", "units": "mm" }"#,
        )
        .unwrap();
        assert_eq!((info.kind.as_str(), info.item_count), ("Excellon Drill", 2));
    }

    #[test]
    fn fab_layers_start_with_copper() {
        let layers = fab_layers(&["F.Cu".into(), "In1.Cu".into(), "B.Cu".into()]);
        assert_eq!(&layers[..3], ["F.Cu", "In1.Cu", "B.Cu"]);
        assert_eq!(layers.last().map(String::as_str), Some("Edge.Cuts"));
    }
}
