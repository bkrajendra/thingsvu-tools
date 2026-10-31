// SPDX-License-Identifier: GPL-3.0-or-later
//! The symbol libraries on this machine (Phase 14.7): the KiCad install's own
//! (`share/kicad/symbols`) and the project's (`sym-lib-table` next to the schematic). A user's global
//! library table is not read.
//!
//! A search reads each library file once as text, line by line (KiCad writes one top-level symbol
//! per `\t(symbol "NAME"` and its fields at two tabs), and keeps what a person searches by: name,
//! description, keywords, default footprint, and its filters. The result is kept for the life of
//! the process, per file and modification time. `get` parses one library properly and describes one
//! symbol: pins, units, fields.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use kicad_cli_runner::KicadInstall;
use kicad_domain::{LibraryId, ToolError};
use serde::Serialize;

/// What a search keeps of one symbol.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SymbolEntry {
    pub library: String,
    pub name: String,
    pub description: String,
    pub keywords: String,
    pub footprint: String,
    pub footprint_filters: String,
    pub datasheet: String,
    /// The symbol this one is derived from, if any.
    pub extends: Option<String>,
    pub units: u32,
    pub pins: usize,
    pub power: bool,
}

impl SymbolEntry {
    pub fn id(&self) -> String {
        format!("{}:{}", self.library, self.name)
    }
}

/// One pin of a symbol, for `get`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PinDetail {
    pub number: String,
    pub name: String,
    pub electrical_type: String,
    /// 0 for all units.
    pub unit: u32,
    /// Where the pin connects, mm from the symbol's origin, y down.
    pub x_mm: f64,
    pub y_mm: f64,
    /// The way the pin points away from the body.
    pub points: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SymbolDetail {
    pub id: String,
    pub entry: SymbolEntry,
    pub reference_prefix: String,
    pub pins: Vec<PinDetail>,
}

/// The quoted string at the start of `rest`, and what follows it.
fn quoted(rest: &str) -> Option<(String, &str)> {
    let rest = rest.trim_start().strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?.1),
            '"' => return Some((out, &rest[i + 1..])),
            c => out.push(c),
        }
    }
    None
}

fn unquote(rest: &str) -> Option<String> {
    quoted(rest).map(|(s, _)| s)
}

/// The symbols of one library file's text.
pub fn scan_library(library: &str, text: &str) -> Vec<SymbolEntry> {
    let mut entries: Vec<SymbolEntry> = Vec::new();
    let mut current: Option<SymbolEntry> = None;
    // Inside a unit of the normal body style (the alternate, De Morgan body repeats the pins).
    let mut normal_body = true;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("\t(symbol ") {
            if let Some(done) = current.take() {
                entries.push(done);
            }
            if let Some(name) = unquote(rest) {
                current = Some(SymbolEntry {
                    library: library.to_owned(),
                    name,
                    description: String::new(),
                    keywords: String::new(),
                    footprint: String::new(),
                    footprint_filters: String::new(),
                    datasheet: String::new(),
                    extends: None,
                    units: 1,
                    pins: 0,
                    power: false,
                });
                normal_body = true;
            }
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        if let Some(rest) = line.strip_prefix("\t\t(property ") {
            let Some((name, after)) = quoted(rest) else { continue };
            let Some(value) = unquote(after) else { continue };
            match name.as_str() {
                "Description" => entry.description = value,
                "ki_keywords" => entry.keywords = value,
                "Footprint" => entry.footprint = value,
                "ki_fp_filters" => entry.footprint_filters = value,
                "Datasheet" => entry.datasheet = value,
                _ => {}
            }
        } else if let Some(rest) = line.strip_prefix("\t\t(extends ") {
            entry.extends = unquote(rest);
        } else if line.starts_with("\t\t(power") {
            entry.power = true;
        } else if let Some(rest) = line.strip_prefix("\t\t(symbol ") {
            // A unit: `NAME_<unit>_<style>`.
            if let Some(sub) = unquote(rest) {
                let mut parts = sub.rsplitn(3, '_');
                let (style, unit) = (parts.next(), parts.next());
                if let (Some(unit), Some(style)) = (unit.and_then(|u| u.parse::<u32>().ok()), style) {
                    normal_body = style == "1" || style == "0";
                    if normal_body {
                        entry.units = entry.units.max(unit);
                    }
                }
            }
        } else if line.starts_with("\t\t\t(pin ") && normal_body {
            entry.pins += 1;
        }
    }
    if let Some(done) = current.take() {
        entries.push(done);
    }
    // A derived symbol has the pins and units of its parent.
    let parents: BTreeMap<String, (usize, u32, bool)> = entries
        .iter()
        .filter(|e| e.extends.is_none())
        .map(|e| (e.name.clone(), (e.pins, e.units, e.power)))
        .collect();
    for e in entries.iter_mut() {
        if let Some((pins, units, power)) = e.extends.as_ref().and_then(|p| parents.get(p)) {
            e.pins = *pins;
            e.units = (*units).max(e.units);
            e.power |= *power;
        }
    }
    entries
}

type Cache = Mutex<BTreeMap<PathBuf, (SystemTime, Arc<Vec<SymbolEntry>>)>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub struct SymbolLibraries {
    /// Library nickname → its `.kicad_sym` file.
    libraries: BTreeMap<String, PathBuf>,
}

impl SymbolLibraries {
    /// The libraries of the newest install, plus the project's own when `project_dir` has a
    /// `sym-lib-table`. `KICAD_SYMBOL_DIR` replaces the install's folder.
    pub fn discover(installs: &[KicadInstall], project_dir: Option<&Path>) -> Self {
        let root = std::env::var_os("KICAD_SYMBOL_DIR").map(PathBuf::from).or_else(|| {
            installs
                .iter()
                .max_by_key(|i| (i.version.series(), i.version.minor, i.version.patch))
                .and_then(|i| i.cli.parent().and_then(Path::parent).map(Path::to_path_buf))
                .map(|r| r.join("share").join("kicad").join("symbols"))
        });
        let mut libraries = BTreeMap::new();
        if let Some(root) = root {
            for entry in std::fs::read_dir(&root).into_iter().flatten().flatten() {
                let path = entry.path();
                if let Some(name) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".kicad_sym"))
                {
                    libraries.insert(name.to_owned(), path);
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

    fn entries(&self, library: &str) -> Arc<Vec<SymbolEntry>> {
        let Some(path) = self.libraries.get(library) else {
            return Arc::new(vec![]);
        };
        let modified = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if let Some((when, entries)) = cache().lock().ok().and_then(|c| c.get(path).cloned()) {
            if when == modified {
                return entries;
            }
        }
        let entries = Arc::new(
            std::fs::read_to_string(path)
                .map(|t| scan_library(library, &t))
                .unwrap_or_default(),
        );
        if let Ok(mut c) = cache().lock() {
            c.insert(path.clone(), (modified, entries.clone()));
        }
        entries
    }

    /// Symbols that match every word of `query` in their name, library, description, or keywords
    /// (ignoring case), best first: an exact name, a name that starts with the query, a name that
    /// contains it, a keyword, then the description. Optionally in one library only.
    pub fn search(&self, query: &str, library: Option<&str>, limit: usize) -> Vec<SymbolEntry> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            return vec![];
        }
        let whole = query.trim().to_lowercase();
        // The first search reads the libraries; they are read side by side.
        let wanted: Vec<&String> = self
            .libraries
            .keys()
            .filter(|l| library.is_none_or(|want| want.eq_ignore_ascii_case(l)))
            .collect();
        let queue = Mutex::new(wanted.iter().copied());
        std::thread::scope(|scope| {
            for _ in 0..wanted.len().min(8) {
                scope.spawn(|| loop {
                    let next = queue.lock().ok().and_then(|mut q| q.next());
                    match next {
                        Some(lib) => drop(self.entries(lib)),
                        None => break,
                    }
                });
            }
        });
        let mut scored: Vec<(u32, usize, SymbolEntry)> = Vec::new();
        for lib in self
            .libraries
            .keys()
            .filter(|l| library.is_none_or(|want| want.eq_ignore_ascii_case(l)))
        {
            for e in self.entries(lib).iter() {
                let (name, desc, keys, lib_l) = (
                    e.name.to_lowercase(),
                    e.description.to_lowercase(),
                    e.keywords.to_lowercase(),
                    e.library.to_lowercase(),
                );
                if !words
                    .iter()
                    .all(|w| name.contains(w) || desc.contains(w) || keys.contains(w) || lib_l.contains(w))
                {
                    continue;
                }
                let rank = if name == whole {
                    0
                } else if name.starts_with(&whole) {
                    1
                } else if words.iter().all(|w| name.contains(w)) {
                    2
                } else if words.iter().all(|w| name.contains(w) || keys.contains(w)) {
                    3
                } else {
                    4
                };
                scored.push((rank, e.name.len(), e.clone()));
            }
        }
        scored.sort_by_cached_key(|(rank, len, e)| (*rank, *len, e.id()));
        scored.into_iter().take(limit.max(1)).map(|(_, _, e)| e).collect()
    }

    /// One symbol by `Library:Name`, with its pins.
    pub fn get(&self, id: &str) -> Result<SymbolDetail, ToolError> {
        let bad = |m: String| ToolError::new(kicad_domain::ErrorCode::SymbolNotFound, m);
        let lib_id = LibraryId::parse(id).map_err(|_| bad(format!("'{id}' is not Library:Name, like Device:R")))?;
        let path = self.libraries.get(lib_id.library()).ok_or_else(|| {
            bad(format!(
                "no symbol library '{}' (sym_search lists them)",
                lib_id.library()
            ))
        })?;
        let text = std::fs::read_to_string(path).map_err(|e| bad(format!("cannot read {}: {e}", path.display())))?;
        let entry = self
            .entries(lib_id.library())
            .iter()
            .find(|e| e.name == lib_id.entry())
            .cloned()
            .ok_or_else(|| bad(format!("no symbol '{}' in {}", lib_id.entry(), lib_id.library())))?;
        let lib = kicad_sexpr::SymbolLibrary::parse(&text).map_err(|e| bad(e.to_string()))?;
        let node = lib
            .embeddable(lib_id.library(), lib_id.entry())
            .map_err(|e| bad(e.to_string()))?;
        let described = kicad_sexpr::sym::describe(&lib_id, &node);
        let pins = described
            .pins
            .iter()
            .map(|p| PinDetail {
                number: p.number.clone(),
                name: p.name.clone(),
                electrical_type: p.electrical_type.clone(),
                unit: p.unit,
                x_mm: p.local.x.to_mm(),
                y_mm: p.local.y.to_mm(),
                points: format!("{:?}", p.outward).to_lowercase(),
            })
            .collect::<Vec<_>>();
        let mut entry = entry;
        entry.pins = pins.len();
        entry.units = described.unit_count;
        Ok(SymbolDetail {
            id: id.to_owned(),
            entry,
            reference_prefix: described.reference_prefix,
            pins,
        })
    }
}

/// Libraries a project defines in its `sym-lib-table`.
fn project_libraries(dir: &Path) -> Vec<(String, PathBuf)> {
    let Some(table) = std::fs::read_to_string(dir.join("sym-lib-table"))
        .ok()
        .and_then(|t| kicad_sexpr::parse(&t).ok())
    else {
        return vec![];
    };
    let mut out = Vec::new();
    for lib in table.children("lib") {
        let (Some(name), Some(uri)) = (lib.child_text("name"), lib.child_text("uri")) else {
            continue;
        };
        let mut uri = uri.replace("${KIPRJMOD}", &dir.to_string_lossy());
        let mut ok = true;
        while let Some(start) = uri.find("${") {
            let Some(end) = uri[start..].find('}').map(|e| start + e) else {
                ok = false;
                break;
            };
            match std::env::var(&uri[start + 2..end]) {
                Ok(v) => uri.replace_range(start..=end, &v),
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            out.push((name.to_owned(), PathBuf::from(uri)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIB: &str = "(kicad_symbol_lib (version 20251024) (generator \"kicad_symbol_editor\")\n\
\t(symbol \"AP1117-15\"\n\
\t\t(property \"Reference\" \"U\" (at 0 0 0))\n\
\t\t(property \"Description\" \"1A Low Dropout regulator, positive, 1.5V fixed output, SOT-223\" (at 0 0 0))\n\
\t\t(property \"ki_keywords\" \"linear regulator ldo fixed positive\" (at 0 0 0))\n\
\t\t(property \"Footprint\" \"Package_TO_SOT_SMD:SOT-223-3_TabPin2\" (at 0 0 0))\n\
\t\t(property \"ki_fp_filters\" \"SOT?223*TabPin2*\" (at 0 0 0))\n\
\t\t(symbol \"AP1117-15_0_1\"\n\
\t\t\t(rectangle (start -5 -5) (end 5 5))\n\
\t\t)\n\
\t\t(symbol \"AP1117-15_1_1\"\n\
\t\t\t(pin power_in line (at 0 7.62 270) (length 2.54) (name \"GND\") (number \"1\"))\n\
\t\t\t(pin power_out line (at 7.62 0 180) (length 2.54) (name \"VO\") (number \"2\"))\n\
\t\t\t(pin power_in line (at -7.62 0 0) (length 2.54) (name \"VI\") (number \"3\"))\n\
\t\t)\n\
\t)\n\
\t(symbol \"AMS1117-3.3\"\n\
\t\t(extends \"AP1117-15\")\n\
\t\t(property \"Description\" \"1A Low Dropout regulator, positive, 3.3V fixed output, SOT-223\" (at 0 0 0))\n\
\t\t(property \"ki_keywords\" \"linear regulator ldo fixed positive\" (at 0 0 0))\n\
\t)\n\
\t(symbol \"R\"\n\
\t\t(property \"Description\" \"Resistor\" (at 0 0 0))\n\
\t\t(property \"ki_keywords\" \"R res resistor\" (at 0 0 0))\n\
\t\t(symbol \"R_1_1\"\n\
\t\t\t(pin passive line (at 0 3.81 270) (length 1.27) (name \"~\") (number \"1\"))\n\
\t\t\t(pin passive line (at 0 -3.81 90) (length 1.27) (name \"~\") (number \"2\"))\n\
\t\t)\n\
\t)\n\
)\n";

    #[test]
    fn a_library_is_scanned_for_what_a_person_searches_by() {
        let entries = scan_library("Regulator_Linear", LIB);
        assert_eq!(entries.len(), 3);
        let ap = &entries[0];
        assert_eq!(ap.name, "AP1117-15");
        assert_eq!(ap.footprint, "Package_TO_SOT_SMD:SOT-223-3_TabPin2");
        assert_eq!(ap.footprint_filters, "SOT?223*TabPin2*");
        assert_eq!((ap.pins, ap.units), (3, 1));
        assert!(ap.keywords.contains("ldo"));
        // A derived symbol has its parent's pins.
        let ams = &entries[1];
        assert_eq!(
            (ams.name.as_str(), ams.extends.as_deref(), ams.pins),
            ("AMS1117-3.3", Some("AP1117-15"), 3)
        );
        assert!(ams.description.contains("3.3V"));
        assert_eq!(entries[2].pins, 2);
    }

    #[test]
    fn a_search_ranks_names_before_descriptions() {
        let dir = std::env::temp_dir().join(format!("thingsvu-sym-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Regulator_Linear.kicad_sym"), LIB).unwrap();
        std::env::set_var("KICAD_SYMBOL_DIR", &dir);
        let libs = SymbolLibraries::discover(&[], None);
        std::env::remove_var("KICAD_SYMBOL_DIR");
        assert_eq!(libs.library_names(), ["Regulator_Linear"]);
        let hits = libs.search("ams1117", None, 10);
        assert_eq!(
            hits.iter().map(|h| h.id()).collect::<Vec<_>>(),
            ["Regulator_Linear:AMS1117-3.3"]
        );
        // By description and keyword.
        let hits = libs.search("ldo 3.3v", None, 10);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(libs.search("resistor", None, 10)[0].id(), "Regulator_Linear:R");
        assert!(libs.search("zzz", None, 10).is_empty());
        assert!(libs.search("ldo", Some("Device"), 10).is_empty(), "only that library");
        let detail = libs.get("Regulator_Linear:AMS1117-3.3").unwrap();
        assert_eq!(detail.pins.len(), 3);
        assert_eq!(detail.reference_prefix, "U");
        assert!(libs.get("Regulator_Linear:Nope").is_err());
        assert!(libs.get("Nope:R").is_err());
        assert!(libs.get("R").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
