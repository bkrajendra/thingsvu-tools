// SPDX-License-Identifier: GPL-3.0-or-later
//! A session of either kind, so tools work the same through a live KiCad 11 connection and
//! through a schematic file (plan §3.1: "selection order per operation").

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kicad_cli_runner::KicadInstall;
use kicad_domain::{
    CapabilitySet, ExistingDocument, LibraryId, LibrarySymbol, NetInfo, OperationPlan, SymbolInfo, SymbolPins,
    ToolError,
};

use crate::file::FileSession;
use crate::mutate::ApplyReport;
use crate::schematic::OpenDocument;
use crate::session::{Provider, Session, SessionStatus};

pub enum AnySession {
    /// A KiCad 11 API connection, headless or to the editor.
    Ipc(Box<Session>),
    /// A schematic file edited while no editor has it open.
    File(Box<FileSession>),
}

impl From<Session> for AnySession {
    fn from(session: Session) -> Self {
        Self::Ipc(Box::new(session))
    }
}

impl From<FileSession> for AnySession {
    fn from(session: FileSession) -> Self {
        Self::File(Box::new(session))
    }
}

impl AnySession {
    pub fn id(&self) -> &str {
        match self {
            Self::Ipc(s) => s.id(),
            Self::File(s) => s.id(),
        }
    }

    pub fn provider(&self) -> Provider {
        match self {
            Self::Ipc(s) => s.provider(),
            Self::File(s) => s.provider(),
        }
    }

    pub fn kicad_version(&self) -> &str {
        match self {
            Self::Ipc(s) => s.kicad_version(),
            Self::File(s) => s.kicad_version(),
        }
    }

    pub fn document_path(&self) -> &Path {
        match self {
            Self::Ipc(s) => s.document_path(),
            Self::File(s) => s.document_path(),
        }
    }

    pub fn capabilities(&self) -> &CapabilitySet {
        match self {
            Self::Ipc(s) => s.capabilities(),
            Self::File(s) => s.capabilities(),
        }
    }

    /// The `kicad-cli` that reads this session's document (ERC, netlist).
    pub fn cli(&self) -> Option<&KicadInstall> {
        match self {
            Self::Ipc(s) => s.cli(),
            Self::File(s) => s.cli(),
        }
    }

    /// The `kicad-cli` that renders PNG images, if there is one (KiCad 10's cannot).
    pub fn render_cli(&self) -> Option<&KicadInstall> {
        match self {
            Self::Ipc(s) => s.cli(),
            Self::File(s) => s.render_cli(),
        }
    }

    pub async fn status(&self) -> SessionStatus {
        match self {
            Self::Ipc(s) => s.status().await,
            Self::File(s) => s.status().await,
        }
    }

    pub async fn close(&self, discard_unsaved: bool) -> Result<bool, ToolError> {
        match self {
            Self::Ipc(s) => s.close(discard_unsaved).await,
            Self::File(s) => s.close(discard_unsaved).await,
        }
    }

    pub async fn open_documents(&self) -> Result<Vec<OpenDocument>, ToolError> {
        match self {
            Self::Ipc(s) => s.open_documents().await,
            Self::File(s) => s.open_documents().await,
        }
    }

    pub async fn library_symbols(&self, ids: &[LibraryId]) -> Result<BTreeMap<LibraryId, LibrarySymbol>, ToolError> {
        match self {
            Self::Ipc(s) => s.library_symbols(ids).await,
            Self::File(s) => s.library_symbols(ids).await,
        }
    }

    pub async fn existing_document(&self) -> Result<ExistingDocument, ToolError> {
        match self {
            Self::Ipc(s) => s.existing_document().await,
            Self::File(s) => s.existing_document().await,
        }
    }

    pub async fn fingerprint(&self) -> Result<String, ToolError> {
        match self {
            Self::Ipc(s) => s.fingerprint().await,
            Self::File(s) => s.fingerprint().await,
        }
    }

    pub async fn list_symbols(&self) -> Result<Vec<SymbolInfo>, ToolError> {
        match self {
            Self::Ipc(s) => s.list_symbols().await,
            Self::File(s) => s.list_symbols().await,
        }
    }

    pub async fn symbol_pins(&self, selector: &str) -> Result<SymbolPins, ToolError> {
        match self {
            Self::Ipc(s) => s.symbol_pins(selector).await,
            Self::File(s) => s.symbol_pins(selector).await,
        }
    }

    pub async fn netlist(&self) -> Result<Vec<NetInfo>, ToolError> {
        match self {
            Self::Ipc(s) => s.netlist().await,
            Self::File(s) => s.netlist().await,
        }
    }

    pub async fn apply_plan(&self, plan: &OperationPlan, expected_fingerprint: &str) -> Result<ApplyReport, ToolError> {
        match self {
            Self::Ipc(s) => s.apply_plan(plan, expected_fingerprint).await,
            Self::File(s) => s.apply_plan(plan, expected_fingerprint).await,
        }
    }

    pub async fn save(&self) -> Result<(), ToolError> {
        match self {
            Self::Ipc(s) => s.save().await,
            Self::File(s) => s.save().await,
        }
    }

    /// A copy of the document (with its project) in `dir`, for tools that read files.
    pub async fn save_copy(&self, dir: &Path) -> Result<PathBuf, ToolError> {
        match self {
            Self::Ipc(s) => s.save_copy(dir).await,
            Self::File(s) => s.save_copy(dir).await,
        }
    }
}
