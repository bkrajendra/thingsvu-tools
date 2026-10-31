// SPDX-License-Identifier: GPL-3.0-or-later
//! Checkpoints of a project (Phase 14.6): copies of its `.kicad_pcb`, `.kicad_sch`, and `.kicad_pro`
//! kept in `.thingsvu/checkpoints/<id>/` next to them, with a manifest. A checkpoint can be
//! listed, compared with another or with the files as they are, and restored.
//!
//! Only the project's root files are covered; sub-sheets and library tables are not copied.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use kicad_domain::changes::{describe_board_changes, describe_design_changes, describe_rules_changes};
use kicad_domain::{ErrorCode, ToolError};
use serde::{Deserialize, Serialize};

/// The files a project's checkpoints cover, by extension.
pub const EXTENSIONS: [&str; 3] = ["kicad_pcb", "kicad_sch", "kicad_pro"];

/// A project: the folder and the name its files share.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub dir: PathBuf,
    pub stem: String,
}

impl Project {
    /// The project a `.kicad_pcb`, `.kicad_sch`, or `.kicad_pro` file belongs to.
    pub fn of(file: &Path) -> Result<Self, ToolError> {
        let file = std::path::absolute(file).map_err(|e| ToolError::new(ErrorCode::InvalidArgument, e.to_string()))?;
        let ext = file.extension().and_then(|e| e.to_str()).unwrap_or_default();
        if !EXTENSIONS.contains(&ext) {
            return Err(ToolError::new(
                ErrorCode::InvalidArgument,
                format!("{} is not a .kicad_pcb, .kicad_sch, or .kicad_pro file", file.display()),
            ));
        }
        Ok(Self {
            dir: file.parent().map(Path::to_path_buf).unwrap_or_default(),
            stem: file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
        })
    }

    pub fn file(&self, ext: &str) -> PathBuf {
        self.dir.join(format!("{}.{ext}", self.stem))
    }

    pub fn store(&self) -> PathBuf {
        self.dir.join(".thingsvu").join("checkpoints")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    /// FNV-1a over the bytes, to notice a copy that was changed.
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub label: String,
    pub created: String,
    pub files: Vec<FileEntry>,
    /// The open board or schematic of a live session was written into the checkpoint, changes that
    /// are not saved included.
    pub unsaved_included: bool,
    pub kicad_version: Option<String>,
}

fn fnv(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn io(what: &str, path: &Path, e: std::io::Error) -> ToolError {
    ToolError::new(ErrorCode::KicadError, format!("cannot {what} {}: {e}", path.display()))
}

/// Seconds since 1970 as `YYYY-MM-DD HH:MM:SS UTC`, with the date part also as an id prefix.
fn utc(secs: u64) -> (String, String) {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let (h, mi, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    (
        format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC"),
        format!("{y:04}{m:02}{d:02}-{h:02}{mi:02}{s:02}"),
    )
}

/// Makes a checkpoint of the project's files. `sources` replaces the file on disk for an
/// extension (the open document of a live session, saved to a scratch copy).
pub fn create(
    project: &Project,
    label: &str,
    sources: &[(&str, PathBuf)],
    unsaved_included: bool,
    kicad_version: Option<String>,
) -> Result<Manifest, ToolError> {
    if label.chars().any(char::is_control) || label.len() > 200 {
        return Err(ToolError::new(
            ErrorCode::InvalidArgument,
            "a label is up to 200 characters without control characters",
        ));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (created, prefix) = utc(now);
    let store = project.store();
    let mut id = prefix.clone();
    for n in 2.. {
        if !store.join(&id).exists() {
            break;
        }
        id = format!("{prefix}-{n}");
    }
    let dir = store.join(&id);
    std::fs::create_dir_all(&dir).map_err(|e| io("create", &dir, e))?;
    let mut files = Vec::new();
    for ext in EXTENSIONS {
        let source = sources
            .iter()
            .find(|(e, _)| *e == ext)
            .map(|(_, p)| p.clone())
            .unwrap_or_else(|| project.file(ext));
        if !source.is_file() {
            continue;
        }
        let bytes = std::fs::read(&source).map_err(|e| io("read", &source, e))?;
        let name = format!("{}.{ext}", project.stem);
        std::fs::write(dir.join(&name), &bytes).map_err(|e| io("write", &dir.join(&name), e))?;
        files.push(FileEntry {
            name,
            size: bytes.len() as u64,
            hash: fnv(&bytes),
        });
    }
    if files.is_empty() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(ToolError::new(
            ErrorCode::InvalidArgument,
            format!("no {}.kicad_pcb, .kicad_sch, or .kicad_pro to keep", project.stem),
        ));
    }
    let manifest = Manifest {
        id,
        label: label.to_owned(),
        created,
        files,
        unsaved_included,
        kicad_version,
    };
    let text =
        serde_json::to_string_pretty(&manifest).map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))?;
    std::fs::write(dir.join("manifest.json"), text).map_err(|e| io("write", &dir.join("manifest.json"), e))?;
    Ok(manifest)
}

/// Every checkpoint of the project, newest first.
pub fn list(project: &Project) -> Vec<Manifest> {
    let Ok(entries) = std::fs::read_dir(project.store()) else {
        return vec![];
    };
    let mut all: Vec<Manifest> = entries
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join("manifest.json")).ok())
        .filter_map(|t| serde_json::from_str(&t).ok())
        .collect();
    all.sort_by(|a, b| b.id.cmp(&a.id));
    all
}

/// One checkpoint by id (`latest` for the newest), with its folder, its files checked against the
/// manifest.
pub fn get(project: &Project, id: &str) -> Result<(Manifest, PathBuf), ToolError> {
    let all = list(project);
    let manifest = if id == "latest" {
        all.into_iter().next()
    } else {
        all.into_iter().find(|m| m.id == id)
    }
    .ok_or_else(|| {
        ToolError::new(
            ErrorCode::InvalidArgument,
            format!("no checkpoint '{id}' for {} (checkpoint_list shows them)", project.stem),
        )
    })?;
    let dir = project.store().join(&manifest.id);
    for f in &manifest.files {
        let bytes = std::fs::read(dir.join(&f.name)).map_err(|e| io("read", &dir.join(&f.name), e))?;
        if fnv(&bytes) != f.hash {
            return Err(ToolError::new(
                ErrorCode::KicadError,
                format!(
                    "checkpoint {} is damaged: {} does not match its record",
                    manifest.id, f.name
                ),
            ));
        }
    }
    Ok((manifest, dir))
}

/// Puts a checkpoint's files back into the project, each by writing a temporary file and renaming
/// it over the original. Returns the names written.
pub fn restore_files(project: &Project, id: &str) -> Result<Vec<String>, ToolError> {
    let (manifest, dir) = get(project, id)?;
    let mut written = Vec::new();
    for f in &manifest.files {
        let target = project.dir.join(&f.name);
        let tmp = project.dir.join(format!("{}.thingsvu-restore", f.name));
        std::fs::copy(dir.join(&f.name), &tmp).map_err(|e| io("copy", &tmp, e))?;
        std::fs::rename(&tmp, &target).map_err(|e| io("replace", &target, e))?;
        written.push(f.name.clone());
    }
    Ok(written)
}

/// The text of the project's files, from a checkpoint or as they are.
#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    pub pcb: Option<String>,
    pub sch: Option<String>,
    pub pro: Option<String>,
}

impl Snapshot {
    pub fn current(project: &Project) -> Self {
        let read = |ext: &str| std::fs::read_to_string(project.file(ext)).ok();
        Self {
            pcb: read("kicad_pcb"),
            sch: read("kicad_sch"),
            pro: read("kicad_pro"),
        }
    }

    pub fn of(project: &Project, dir: &Path) -> Self {
        let read = |ext: &str| std::fs::read_to_string(dir.join(format!("{}.{ext}", project.stem))).ok();
        Self {
            pcb: read("kicad_pcb"),
            sch: read("kicad_sch"),
            pro: read("kicad_pro"),
        }
    }
}

/// What differs between two snapshots, in words.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Diff {
    pub board: Vec<String>,
    pub nets: Vec<String>,
    pub schematic: Vec<String>,
    pub rules: Vec<String>,
    /// Files that are not in both, or that differ byte for byte.
    pub files_differing: Vec<String>,
    /// Something could not be compared.
    pub notes: Vec<String>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.board.is_empty()
            && self.nets.is_empty()
            && self.schematic.is_empty()
            && self.rules.is_empty()
            && self.files_differing.is_empty()
    }
}

pub fn diff(from: &Snapshot, to: &Snapshot) -> Diff {
    let mut d = Diff::default();
    for (name, a, b) in [
        ("kicad_pcb", &from.pcb, &to.pcb),
        ("kicad_sch", &from.sch, &to.sch),
        ("kicad_pro", &from.pro, &to.pro),
    ] {
        match (a, b) {
            (Some(x), Some(y)) if x != y => d.files_differing.push(format!("{name} differs")),
            (Some(_), None) => d.files_differing.push(format!("{name} is only in the first")),
            (None, Some(_)) => d.files_differing.push(format!("{name} is only in the second")),
            _ => {}
        }
    }
    if let (Some(a), Some(b)) = (&from.pcb, &to.pcb) {
        match (kicad_sexpr::pcb::read_board(a), kicad_sexpr::pcb::read_board(b)) {
            (Ok(x), Ok(y)) => {
                d.board = describe_board_changes(&x, &y);
                let (old, new): (std::collections::BTreeSet<&String>, std::collections::BTreeSet<&String>) =
                    (x.nets.iter().collect(), y.nets.iter().collect());
                for n in new.difference(&old) {
                    d.nets.push(format!("net {n} added"));
                }
                for n in old.difference(&new) {
                    d.nets.push(format!("net {n} removed"));
                }
                // Tracks per net, where the count changed.
                let count = |b: &kicad_domain::board::Board| {
                    let mut m: std::collections::BTreeMap<String, usize> = Default::default();
                    for t in &b.tracks {
                        *m.entry(t.net.clone()).or_default() += 1;
                    }
                    m
                };
                let (cx, cy) = (count(&x), count(&y));
                for net in cx.keys().chain(cy.keys()).collect::<std::collections::BTreeSet<_>>() {
                    let (p, q) = (cx.get(net).copied().unwrap_or(0), cy.get(net).copied().unwrap_or(0));
                    if p != q {
                        d.nets.push(format!(
                            "net {} has {q} track(s), was {p}",
                            if net.is_empty() { "(none)" } else { net }
                        ));
                    }
                }
            }
            _ => d
                .notes
                .push("a board file could not be read, so the boards are not compared".into()),
        }
    }
    if let (Some(a), Some(b)) = (&from.sch, &to.sch) {
        match (
            kicad_sexpr::Document::parse(a.clone()),
            kicad_sexpr::Document::parse(b.clone()),
        ) {
            (Ok(x), Ok(y)) => d.schematic = describe_design_changes(&x.design(), &y.design()),
            _ => d
                .notes
                .push("a schematic file could not be read, so the schematics are not compared".into()),
        }
    }
    if let (Some(a), Some(b)) = (&from.pro, &to.pro) {
        match (serde_json::from_str(a), serde_json::from_str(b)) {
            (Ok(x), Ok(y)) => {
                d.rules = describe_rules_changes(&crate::rules_from_project(&x), &crate::rules_from_project(&y))
            }
            _ => d
                .notes
                .push("a project file could not be read, so the rules are not compared".into()),
        }
    }
    if !d.files_differing.is_empty()
        && d.board.is_empty()
        && d.nets.is_empty()
        && d.schematic.is_empty()
        && d.rules.is_empty()
    {
        d.notes.push(
            "the files differ, but not in parts, nets, tracks, zones, or rules (formatting, text, or something this comparison does not cover)"
                .into(),
        );
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("thingsvu-checkpoint-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn dates_are_utc() {
        assert_eq!(utc(0).0, "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(1_730_000_000).1, "20241027-033320");
        assert_eq!(utc(951_782_400).0, "2000-02-29 00:00:00 UTC", "a leap day");
    }

    #[test]
    fn a_checkpoint_is_made_listed_checked_and_restored() {
        let dir = temp("round");
        let project = Project::of(&dir.join("board.kicad_pcb")).unwrap();
        assert!(create(&project, "empty", &[], false, None).is_err(), "nothing to keep");
        std::fs::write(project.file("kicad_pcb"), "(kicad_pcb one)").unwrap();
        std::fs::write(project.file("kicad_pro"), "{\"a\":1}").unwrap();
        let first = create(&project, "before the rip-up", &[], false, Some("10.99".into())).unwrap();
        assert_eq!(first.files.len(), 2);
        // An open document replaces the file on disk.
        let live = dir.join("live.kicad_pcb");
        std::fs::write(&live, "(kicad_pcb two)").unwrap();
        let second = create(&project, "unsaved included", &[("kicad_pcb", live)], true, None).unwrap();
        assert_ne!(first.id, second.id, "ids do not repeat within a second");
        let all = list(&project);
        assert_eq!(
            all.iter().map(|m| m.label.as_str()).collect::<Vec<_>>(),
            ["unsaved included", "before the rip-up"]
        );
        assert!(all[0].unsaved_included);

        std::fs::write(project.file("kicad_pcb"), "(kicad_pcb three)").unwrap();
        let written = restore_files(&project, &first.id).unwrap();
        assert_eq!(written, ["board.kicad_pcb", "board.kicad_pro"]);
        assert_eq!(
            std::fs::read_to_string(project.file("kicad_pcb")).unwrap(),
            "(kicad_pcb one)"
        );
        assert!(!project.dir.join("board.kicad_pcb.thingsvu-restore").exists());
        assert_eq!(get(&project, "latest").unwrap().0.id, second.id);

        // A damaged copy is noticed.
        std::fs::write(project.store().join(&first.id).join("board.kicad_pcb"), "(tampered)").unwrap();
        assert!(restore_files(&project, &first.id)
            .unwrap_err()
            .message
            .contains("damaged"));
        assert!(get(&project, "nope").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_states_are_compared_by_meaning() {
        let a = Snapshot {
            pro: Some(r#"{"net_settings":{"classes":[{"name":"Default","track_width":0.2,"priority":1}]}}"#.into()),
            ..Default::default()
        };
        let b = Snapshot {
            pro: Some(r#"{"net_settings":{"classes":[{"name":"Default","track_width":0.25,"priority":1}]}}"#.into()),
            ..Default::default()
        };
        let d = diff(&a, &b);
        assert_eq!(d.rules, ["class 'Default' track width 0.2 mm -> 0.25 mm"]);
        assert_eq!(d.files_differing, ["kicad_pro differs"]);
        assert!(diff(&a, &a).is_empty());
        let only = diff(&a, &Snapshot::default());
        assert_eq!(only.files_differing, ["kicad_pro is only in the first"]);
    }
}
