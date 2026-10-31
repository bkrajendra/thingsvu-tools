// SPDX-License-Identifier: GPL-3.0-or-later
//! Has the file on disk changed since a session read it (Phase 14.5)? Another program, a version
//! control checkout, or the editor's own save can change it under a session that holds the board
//! in memory.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

use serde::Serialize;

use crate::session::Provider;

/// What a reload did.
#[derive(Debug, Clone, Serialize)]
pub struct ReloadReport {
    pub reloaded: bool,
    pub provider: Provider,
    /// Whether the open document had changes that were not saved.
    pub had_unsaved_changes: Option<bool>,
    /// Whether the file on disk had changed since the session read it.
    pub file_changed_on_disk: Option<bool>,
    /// What the open document had that the file has not, which the reload dropped.
    pub dropped: Vec<String>,
    pub counts_after: BTreeMap<&'static str, usize>,
    pub note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl FileStamp {
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        })
    }
}

/// The stamp of a document's file when a session last read or wrote it.
#[derive(Debug)]
pub struct Freshness(Mutex<Option<FileStamp>>);

impl Freshness {
    pub fn new(path: &Path) -> Self {
        Self(Mutex::new(FileStamp::of(path)))
    }

    /// The file is read or written now.
    pub fn reset(&self, path: &Path) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = FileStamp::of(path);
    }

    /// Has the file changed since? `None` when it cannot be told (no stamp, or the file is gone).
    pub fn changed(&self, path: &Path) -> Option<bool> {
        let was = (*self.0.lock().unwrap_or_else(|e| e.into_inner()))?;
        Some(FileStamp::of(path)? != was)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_to_the_file_is_noticed_until_the_session_reads_it_again() {
        let dir = std::env::temp_dir().join(format!("thingsvu-fresh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.kicad_pcb");
        std::fs::write(&file, "one").unwrap();
        let fresh = Freshness::new(&file);
        assert_eq!(fresh.changed(&file), Some(false));
        std::fs::write(&file, "one and more").unwrap();
        assert_eq!(fresh.changed(&file), Some(true));
        fresh.reset(&file);
        assert_eq!(fresh.changed(&file), Some(false));
        std::fs::remove_file(&file).unwrap();
        assert_eq!(fresh.changed(&file), None, "a missing file cannot be compared");
        assert_eq!(Freshness::new(&dir.join("none")).changed(&file), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
