// SPDX-License-Identifier: GPL-3.0-or-later
//! The footprint libraries on this machine: the KiCad install's own (`share/kicad/footprints`) and
//! the project's (`fp-lib-table` next to the schematic). A user's global library table is not
//! read, as for symbols.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kicad_cli_runner::KicadInstall;
use kicad_domain::FootprintInfo;
use serde::Serialize;

/// One footprint a search found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FootprintHit {
    pub id: String,
    pub pads: usize,
    pub description: String,
    pub tags: String,
}

pub struct FootprintLibraries {
    /// Library nickname → its `.pretty` folder.
    libraries: BTreeMap<String, PathBuf>,
}

fn nickname(dir: &Path) -> Option<String> {
    let name = dir.file_name()?.to_str()?;
    name.strip_suffix(".pretty").map(str::to_owned)
}

impl FootprintLibraries {
    /// The libraries of the newest install, plus the project's own when `project_dir` has an
    /// `fp-lib-table`. `KICAD_FOOTPRINT_DIR` replaces the install's folder (for tests).
    pub fn discover(installs: &[KicadInstall], project_dir: Option<&Path>) -> Self {
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(dir) = std::env::var_os("KICAD_FOOTPRINT_DIR") {
            roots.push(PathBuf::from(dir));
        } else if let Some(install) = installs
            .iter()
            .max_by_key(|i| (i.version.series(), i.version.minor, i.version.patch))
        {
            if let Some(root) = install.cli.parent().and_then(Path::parent) {
                roots.push(root.join("share").join("kicad").join("footprints"));
            }
        }
        let mut libraries = BTreeMap::new();
        for root in roots {
            if let Ok(entries) = std::fs::read_dir(&root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if let Some(name) = nickname(&path) {
                        libraries.insert(name, path);
                    }
                }
            }
        }
        if let Some(dir) = project_dir {
            for (name, path) in project_libraries(dir) {
                libraries.insert(name, path);
            }
        }
        Self { libraries }
    }

    pub fn library_names(&self) -> Vec<&str> {
        self.libraries.keys().map(String::as_str).collect()
    }

    fn entries(&self, library: &str) -> Vec<String> {
        let Some(dir) = self.libraries.get(library) else {
            return vec![];
        };
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.strip_suffix(".kicad_mod").map(str::to_owned))
            .collect();
        names.sort();
        names
    }

    /// One footprint, by `Library:Name`.
    pub fn get(&self, id: &str) -> Option<FootprintInfo> {
        let (library, name) = id.split_once(':')?;
        // Names are not paths.
        if name.contains(['/', '\\']) || name.starts_with('.') {
            return None;
        }
        let path = self.libraries.get(library)?.join(format!("{name}.kicad_mod"));
        let text = std::fs::read_to_string(path).ok()?;
        kicad_sexpr::fp::parse_footprint(id, &text).ok()
    }

    /// Footprints whose `Library:Name` contains every word of `query` (ignoring case, dashes, and
    /// underscores), shortest names first, optionally only in one library and with a given number of
    /// pads. Reads at most 400 files to count pads.
    pub fn search(&self, query: &str, library: Option<&str>, pads: Option<usize>, limit: usize) -> Vec<FootprintHit> {
        let squash = |s: &str| s.to_ascii_lowercase().replace(['-', '_', ' '], "");
        let words: Vec<String> = query.split_whitespace().map(squash).filter(|w| !w.is_empty()).collect();
        let mut candidates: Vec<String> = Vec::new();
        for lib in self
            .libraries
            .keys()
            .filter(|l| library.is_none_or(|want| want.eq_ignore_ascii_case(l)))
        {
            for name in self.entries(lib) {
                let id = format!("{lib}:{name}");
                let flat = squash(&id);
                if words.iter().all(|w| flat.contains(w)) {
                    candidates.push(id);
                }
            }
        }
        candidates.sort_by_key(|id| (id.len(), id.clone()));
        candidates.truncate(if pads.is_some() { 400 } else { limit.max(1) });
        candidates
            .into_iter()
            .filter_map(|id| {
                let fp = self.get(&id)?;
                let count = fp.pad_numbers().len();
                pads.is_none_or(|p| p == count).then_some(FootprintHit {
                    id,
                    pads: count,
                    description: fp.description,
                    tags: fp.tags,
                })
            })
            .take(limit.max(1))
            .collect()
    }
}

/// Libraries named in a project's `fp-lib-table` whose location is inside the project.
fn project_libraries(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(text) = std::fs::read_to_string(dir.join("fp-lib-table")) else {
        return vec![];
    };
    let Ok(table) = kicad_sexpr::parse(&text) else {
        return vec![];
    };
    table
        .children("lib")
        .filter_map(|lib| {
            let name = lib.child_text("name")?.to_owned();
            let uri = lib.child_text("uri")?;
            let path = uri.replace("${KIPRJMOD}", &dir.to_string_lossy());
            let path = PathBuf::from(path);
            let path = if path.is_absolute() { path } else { dir.join(path) };
            path.is_dir().then_some((name, path))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library_with(footprints: &[(&str, usize)]) -> (PathBuf, FootprintLibraries) {
        let root = std::env::temp_dir().join(format!("thingsvu-fp-{}-{}", std::process::id(), footprints.len()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("Resistor_SMD.pretty");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, pads) in footprints {
            let pad_text: String = (1..=*pads)
                .map(|n| format!("(pad \"{n}\" smd rect (at {n} 0) (size 1 1) (layers \"F.Cu\"))\n"))
                .collect();
            std::fs::write(
                dir.join(format!("{name}.kicad_mod")),
                format!("(footprint \"{name}\" (descr \"test {name}\") (tags \"t\") {pad_text})"),
            )
            .unwrap();
        }
        let mut libraries = BTreeMap::new();
        libraries.insert("Resistor_SMD".to_owned(), dir);
        (root, FootprintLibraries { libraries })
    }

    #[test]
    fn a_footprint_is_found_by_the_words_of_its_name() {
        let (_root, libs) = library_with(&[
            ("R_0603_1608Metric", 2),
            ("R_0805_2012Metric", 2),
            ("R_Array_Convex_4x0603", 8),
        ]);
        let hits = libs.search("r 0603", None, None, 10);
        assert_eq!(
            hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            ["Resistor_SMD:R_0603_1608Metric", "Resistor_SMD:R_Array_Convex_4x0603"]
        );
        assert_eq!(hits[0].pads, 2);
        let eight = libs.search("0603", None, Some(8), 10);
        assert_eq!(eight.len(), 1);
        assert_eq!(eight[0].pads, 8);
        assert!(libs.search("nothing like this", None, None, 10).is_empty());
        assert!(libs.search("0603", Some("Other"), None, 10).is_empty());
    }

    #[test]
    fn footprints_are_read_by_id_and_names_are_not_paths() {
        let (_root, libs) = library_with(&[("R_0603_1608Metric", 2)]);
        let fp = libs.get("Resistor_SMD:R_0603_1608Metric").unwrap();
        assert_eq!(fp.pad_numbers().len(), 2);
        assert_eq!(fp.description, "test R_0603_1608Metric");
        assert!(libs.get("Resistor_SMD:../x").is_none());
        assert!(libs.get("Missing:R_0603_1608Metric").is_none());
        assert!(libs.get("no-colon").is_none());
    }
}
