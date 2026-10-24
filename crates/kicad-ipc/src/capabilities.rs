// SPDX-License-Identifier: GPL-3.0-or-later
//! Side-effect-free probing of which API commands a running KiCad handles.
//!
//! Each probe sends a request that cannot change the design: a read, an empty batch, a request
//! for a symbol that does not exist, or a commit that is opened and immediately dropped. KiCad
//! answering `AS_UNHANDLED` / `AS_UNIMPLEMENTED` means the command is missing; any other answer,
//! including a validation error, means a handler exists.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::client::{Client, Error};
use crate::proto::kiapi::common::commands::{
    BeginCommit, BeginCommitResponse, CommitAction, CreateItems, EndCommit, GetDocumentModifiedState, GetItems,
    GetOpenDocuments, GetOpenDocumentsResponse, GetVersion, GetVersionResponse,
};
use crate::proto::kiapi::common::types::{
    DocumentSpecifier, DocumentType, ItemHeader, KiCadObjectType, LibraryIdentifier,
};
use crate::proto::kiapi::schematic::commands::{GetSchematicHierarchy, GetSchematicNetlist, PlaceSymbolFromLibrary};

/// Result of probing one command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum CommandSupport {
    Supported,
    Unhandled,
    /// The probe could not decide (timeout, transport failure, unexpected error).
    Unknown(String),
}

impl CommandSupport {
    pub fn is_supported(&self) -> bool {
        matches!(self, CommandSupport::Supported)
    }
}

/// Probed commands, by protobuf message name.
pub mod command {
    pub const GET_OPEN_DOCUMENTS: &str = "GetOpenDocuments";
    pub const GET_SCHEMATIC_HIERARCHY: &str = "GetSchematicHierarchy";
    pub const GET_SCHEMATIC_NETLIST: &str = "GetSchematicNetlist";
    pub const PLACE_SYMBOL_FROM_LIBRARY: &str = "PlaceSymbolFromLibrary";
    pub const CREATE_ITEMS: &str = "CreateItems";
    pub const BEGIN_COMMIT: &str = "BeginCommit";
    pub const GET_DOCUMENT_MODIFIED_STATE: &str = "GetDocumentModifiedState";
    pub const GET_ITEMS: &str = "GetItems";
}

#[derive(Debug, Clone, Serialize)]
pub struct ProbeReport {
    pub kicad_version: String,
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    /// Instance token pinned from the first reply (identifies this KiCad instance).
    #[serde(skip)]
    pub instance_token: Option<String>,
    /// Open schematic documents, if the schematic editor or headless server has any.
    #[serde(skip)]
    pub schematics: Vec<DocumentSpecifier>,
    pub commands: BTreeMap<&'static str, CommandSupport>,
}

impl ProbeReport {
    pub fn supports(&self, command: &str) -> bool {
        self.commands.get(command).is_some_and(CommandSupport::is_supported)
    }
}

fn classify<T>(result: Result<T, Error>) -> CommandSupport {
    match result {
        Ok(_) => CommandSupport::Supported,
        Err(e) if e.is_unhandled() => CommandSupport::Unhandled,
        // A handler that rejected our deliberately invalid request still exists.
        Err(Error::Status { .. }) => CommandSupport::Supported,
        Err(e) => CommandSupport::Unknown(e.to_string()),
    }
}

/// Probes the session behind `client`. Document-level probes run against the first open
/// schematic; without one they are reported as `Unknown`.
pub async fn probe(client: &mut Client) -> Result<ProbeReport, Error> {
    let version: GetVersionResponse = client.call(&GetVersion {}).await?;
    let v = version.version.unwrap_or_default();
    let mut commands = BTreeMap::new();

    let docs = client
        .call::<_, GetOpenDocumentsResponse>(&GetOpenDocuments {
            r#type: DocumentType::DoctypeSchematic as i32,
        })
        .await;
    let schematics = match &docs {
        Ok(r) => r.documents.clone(),
        Err(_) => Vec::new(),
    };
    commands.insert(command::GET_OPEN_DOCUMENTS, classify(docs));

    let mut report = ProbeReport {
        kicad_version: v.full_version,
        major: v.major,
        minor: v.minor,
        patch: v.patch,
        instance_token: client.instance_token().map(str::to_owned),
        schematics,
        commands,
    };

    let Some(doc) = report.schematics.first().cloned() else {
        // GetOpenDocuments is unhandled when nothing is loaded, so document probes cannot run.
        // A server whose schematic handler exists but has no document still answers it with
        // an empty list; the caller distinguishes the two through `commands`.
        let why = || CommandSupport::Unknown("no open schematic to probe against".into());
        for name in [
            command::GET_SCHEMATIC_HIERARCHY,
            command::GET_SCHEMATIC_NETLIST,
            command::PLACE_SYMBOL_FROM_LIBRARY,
            command::CREATE_ITEMS,
            command::BEGIN_COMMIT,
            command::GET_DOCUMENT_MODIFIED_STATE,
            command::GET_ITEMS,
        ] {
            report.commands.insert(name, why());
        }
        return Ok(report);
    };
    let header = Some(ItemHeader {
        document: Some(doc.clone()),
        ..Default::default()
    });

    let r = client
        .call_any(&GetSchematicHierarchy {
            document: Some(doc.clone()),
        })
        .await;
    report.commands.insert(command::GET_SCHEMATIC_HIERARCHY, classify(r));
    let r = client
        .call_any(&GetSchematicNetlist {
            document: Some(doc.clone()),
            ..Default::default()
        })
        .await;
    report.commands.insert(command::GET_SCHEMATIC_NETLIST, classify(r));
    let r = client
        .call_any(&GetDocumentModifiedState {
            document: Some(doc.clone()),
        })
        .await;
    report
        .commands
        .insert(command::GET_DOCUMENT_MODIFIED_STATE, classify(r));
    let r = client
        .call_any(&GetItems {
            header: header.clone(),
            types: vec![KiCadObjectType::KotSchSymbol as i32],
        })
        .await;
    report.commands.insert(command::GET_ITEMS, classify(r));

    // A library entry that cannot exist: a real handler answers "not found" and places nothing.
    let r = client
        .call_any(&PlaceSymbolFromLibrary {
            header: header.clone(),
            lib_id: Some(LibraryIdentifier {
                library_nickname: "thingsvu-probe-nonexistent".into(),
                entry_name: "none".into(),
            }),
            ..Default::default()
        })
        .await;
    report.commands.insert(command::PLACE_SYMBOL_FROM_LIBRARY, classify(r));

    // An empty batch creates nothing.
    let r = client
        .call_any(&CreateItems {
            header: header.clone(),
            ..Default::default()
        })
        .await;
    report.commands.insert(command::CREATE_ITEMS, classify(r));

    // Open and immediately drop a commit; nothing is applied.
    let begin = client
        .call::<_, BeginCommitResponse>(&BeginCommit { header: header.clone() })
        .await;
    if let Ok(opened) = &begin {
        client
            .call_any(&EndCommit {
                id: opened.id.clone(),
                action: CommitAction::CmaDrop as i32,
                message: String::new(),
                header: header.clone(),
            })
            .await?;
    }
    report.commands.insert(command::BEGIN_COMMIT, classify(begin));

    // Export jobs are not probed: any request a handler accepts would write a file. Their support
    // is learned on first use.
    Ok(report)
}
