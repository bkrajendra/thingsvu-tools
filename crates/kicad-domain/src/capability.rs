// SPDX-License-Identifier: GPL-3.0-or-later
//! What a session can do, determined by probing the running KiCad — never assumed from a version
//! number alone (plan §3.2–3.3).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Capability {
    #[serde(rename = "session.status")]
    SessionStatus,
    #[serde(rename = "session.open_documents")]
    SessionOpenDocuments,
    #[serde(rename = "session.document_lifecycle")]
    DocumentLifecycle,
    #[serde(rename = "session.commits")]
    Commits,
    #[serde(rename = "sch.read_hierarchy")]
    SchReadHierarchy,
    #[serde(rename = "sch.read_symbols")]
    SchReadSymbols,
    #[serde(rename = "sch.netlist")]
    SchNetlist,
    #[serde(rename = "sch.place_symbol")]
    SchPlaceSymbol,
    #[serde(rename = "sch.edit_items")]
    SchEditItems,
    #[serde(rename = "sch.save")]
    SchSave,
    #[serde(rename = "sch.erc")]
    SchErc,
    #[serde(rename = "sch.render")]
    SchRender,
    #[serde(rename = "sch.export_netlist")]
    SchExportNetlist,
}

impl Capability {
    /// Every capability, in the order the capability matrix lists them.
    pub const ALL: [Capability; 13] = [
        Capability::SessionStatus,
        Capability::SessionOpenDocuments,
        Capability::DocumentLifecycle,
        Capability::Commits,
        Capability::SchReadHierarchy,
        Capability::SchReadSymbols,
        Capability::SchNetlist,
        Capability::SchPlaceSymbol,
        Capability::SchEditItems,
        Capability::SchSave,
        Capability::SchErc,
        Capability::SchRender,
        Capability::SchExportNetlist,
    ];
}

/// Where a capability is served from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    KicadIpc,
    KicadCli,
    ProjectFile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Support {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<Backend>,
    /// Why it is unavailable, or a caveat when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Support {
    pub fn via(backend: Backend) -> Self {
        Self {
            available: true,
            backend: Some(backend),
            note: None,
        }
    }

    pub fn unavailable(note: impl Into<String>) -> Self {
        Self {
            available: false,
            backend: None,
            note: Some(note.into()),
        }
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilitySet(BTreeMap<Capability, Support>);

impl CapabilitySet {
    pub fn set(&mut self, capability: Capability, support: Support) {
        self.0.insert(capability, support);
    }

    pub fn get(&self, capability: Capability) -> Option<&Support> {
        self.0.get(&capability)
    }

    pub fn is_available(&self, capability: Capability) -> bool {
        self.get(capability).is_some_and(|s| s.available)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Capability, &Support)> {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docs/capability-matrix.md has a row for every capability, so it cannot fall behind the code.
    #[test]
    fn the_capability_matrix_lists_every_capability() {
        let doc = include_str!("../../../docs/capability-matrix.md");
        for capability in Capability::ALL {
            let name = serde_json::to_value(capability).unwrap();
            let name = name.as_str().unwrap();
            assert!(
                doc.contains(&format!("| `{name}` |")),
                "{name} has no row in docs/capability-matrix.md"
            );
        }
        // Nothing was added to the enum without being added to ALL: every name round-trips and
        // the list has no duplicates.
        let unique: std::collections::BTreeSet<_> = Capability::ALL.iter().collect();
        assert_eq!(unique.len(), Capability::ALL.len());
    }

    #[test]
    fn serializes_with_dotted_names() {
        let mut caps = CapabilitySet::default();
        caps.set(Capability::SchErc, Support::via(Backend::KicadCli));
        caps.set(
            Capability::Commits,
            Support::unavailable("headless server has no commit support"),
        );
        let json = serde_json::to_value(&caps).unwrap();
        assert_eq!(json["sch.erc"]["backend"], "kicad_cli");
        assert_eq!(json["session.commits"]["available"], false);
        assert!(caps.is_available(Capability::SchErc));
        assert!(!caps.is_available(Capability::SchPlaceSymbol));
    }
}
