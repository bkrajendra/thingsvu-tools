// SPDX-License-Identifier: GPL-3.0-or-later
//! Board sessions (Phase 11): a `.kicad_pcb` open in a headless KiCad server this process owns, in
//! a PCB editor that registered itself through the plugin, or read straight from the file.
//!
//! A board session is separate from a schematic session: it has its own tools (`pcb_*`), its own
//! plan store entries, and a smaller capability set. KiCad 11 serves reading, importing the
//! netlist, and (later) editing; KiCad 10 serves reading the file, DRC, and exports.

use std::path::{Path, PathBuf};

use kicad_cli_runner::KicadInstall;
use kicad_domain::{ErrorCode, ToolError};
use kicad_ipc::proto::kiapi::board::commands::{ImportNetlist, ImportNetlistResponse, NetlistMatchMode};
use kicad_ipc::proto::kiapi::common::commands::{
    CloseDocument, DocumentModifiedState, GetDocumentModifiedState, GetDocumentModifiedStateResponse, GetOpenDocuments,
    GetOpenDocumentsResponse, LoadAllLibraries, OpenDocument, OpenDocumentResponse, Ping, SaveCopyOfDocument,
    SaveDocument,
};
use kicad_ipc::proto::kiapi::common::types::{DocumentSpecifier, DocumentType, LibraryType};
use kicad_ipc::Client;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::errors::from_ipc_error;
use crate::headless::HeadlessServer;
use crate::session::{new_session_id, Provider, SessionConfig};

mod edit;
pub use edit::{BoardSnapshot, EditReport, RestoreReport, SessionImport};

/// What `pcb_update_from_schematic` reports, from a dry run or the real import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    pub errors: u32,
    pub warnings: u32,
    pub new_footprints: u32,
    /// KiCad's own report of the changes, one per line.
    pub report: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BoardStatus {
    pub session_id: String,
    pub provider: Provider,
    pub kicad_version: String,
    pub document: PathBuf,
    pub connected: bool,
    pub unsaved_changes: Option<bool>,
    pub server_pid: Option<u32>,
    /// What this session can do, in words.
    pub can: Vec<&'static str>,
}

struct Ipc {
    client: Client,
    document: DocumentSpecifier,
    server: Option<HeadlessServer>,
}

pub struct BoardSession {
    id: String,
    provider: Provider,
    kicad_version: String,
    document_path: PathBuf,
    cli: Option<KicadInstall>,
    /// A KiCad 11-series CLI for 3D renders, if one is installed.
    render_cli: Option<KicadInstall>,
    ipc: Option<Mutex<Ipc>>,
}

fn check_board_path(path: &Path) -> Result<PathBuf, ToolError> {
    let path = std::path::absolute(path).map_err(|e| ToolError::new(ErrorCode::InvalidArgument, e.to_string()))?;
    if path.extension().and_then(|e| e.to_str()) != Some("kicad_pcb") {
        return Err(ToolError::new(
            ErrorCode::InvalidArgument,
            format!("{} is not a .kicad_pcb file", path.display()),
        ));
    }
    if !path.is_file() {
        return Err(ToolError::new(
            ErrorCode::InvalidArgument,
            format!("{} does not exist", path.display()),
        ));
    }
    Ok(path)
}

impl BoardSession {
    /// Opens the board in a private headless KiCad 11 server.
    pub async fn open_headless(
        config: &SessionConfig,
        installs: &[KicadInstall],
        path: &Path,
    ) -> Result<Self, ToolError> {
        let path = check_board_path(path)?;
        if crate::session::lock_file(&path).is_some_and(|l| l.exists()) {
            return Err(ToolError::new(
                ErrorCode::DocumentOpenElsewhere,
                format!(
                    "{} is open in a KiCad editor (lock file present); close it there first",
                    path.display()
                ),
            ));
        }
        let cli = installs
            .iter()
            .filter(|i| i.version.series() >= 11)
            .max_by_key(|i| (i.version.major, i.version.minor, i.version.patch))
            .cloned()
            .ok_or_else(|| {
                ToolError::new(
                    ErrorCode::KicadNotFound,
                    "headless board sessions need KiCad 11 (or a 10.99 nightly) with kicad-cli; none was found",
                )
            })?;
        std::fs::create_dir_all(&config.runtime_dir).map_err(|e| {
            ToolError::new(
                ErrorCode::KicadError,
                format!("cannot create {}: {e}", config.runtime_dir.display()),
            )
        })?;
        let id = new_session_id();
        let socket = config.runtime_dir.join(format!("{id}.sock"));
        let (server, mut client) = HeadlessServer::start(
            &cli.cli,
            socket,
            &format!("thingsvu-kicad-ai-{id}"),
            config.start_timeout,
            config.request_timeout,
        )
        .await?;
        let opened: OpenDocumentResponse = client
            .call(&OpenDocument {
                r#type: DocumentType::DoctypePcb as i32,
                path: path.display().to_string(),
            })
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        let document = opened.document.ok_or_else(|| {
            ToolError::new(
                ErrorCode::KicadError,
                "KiCad opened the board but returned no identifier",
            )
        })?;
        let version = cli.version.full.clone();
        Ok(Self {
            id,
            provider: Provider::Headless,
            kicad_version: version,
            document_path: path,
            render_cli: Some(cli.clone()),
            cli: Some(cli),
            ipc: Some(Mutex::new(Ipc {
                client,
                document,
                server: Some(server),
            })),
        })
    }

    /// Connects to the PCB editor that registered itself, for the board at `path` (or the only one
    /// open). Nothing is started or stopped.
    pub async fn open_gui(
        config: &SessionConfig,
        installs: &[KicadInstall],
        registration: &crate::registry::Registration,
        path: Option<&Path>,
    ) -> Result<Self, ToolError> {
        let id = new_session_id();
        let mut client = Client::connect(
            &registration.socket_path,
            &format!("thingsvu-kicad-ai-{id}"),
            config.request_timeout,
        )
        .await
        .map_err(|e| {
            ToolError::new(
                ErrorCode::NoSession,
                format!("the registered KiCad is not reachable ({e}); press the KiCad AI button in KiCad again"),
            )
        })?;
        client.pin_instance_token(registration.token.clone());
        let docs: GetOpenDocumentsResponse = client
            .call(&GetOpenDocuments {
                r#type: DocumentType::DoctypePcb as i32,
            })
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        let board_of = |d: &DocumentSpecifier| {
            d.project
                .as_ref()
                .map(|p| PathBuf::from(&p.path).join(format!("{}.kicad_pcb", p.name)))
        };
        let candidates: Vec<(DocumentSpecifier, PathBuf)> = docs
            .documents
            .iter()
            .filter_map(|d| board_of(d).map(|p| (d.clone(), p)))
            .collect();
        let norm = |p: &Path| p.display().to_string().to_lowercase();
        let (document, document_path) =
            match path {
                Some(wanted) => {
                    let wanted = std::path::absolute(wanted)
                        .map_err(|e| ToolError::new(ErrorCode::InvalidArgument, e.to_string()))?;
                    candidates
                        .into_iter()
                        .find(|(_, p)| norm(p) == norm(&wanted))
                        .ok_or_else(|| {
                            ToolError::new(
                                ErrorCode::DocumentNotOpen,
                                format!("{} is not open in that KiCad's PCB editor", wanted.display()),
                            )
                        })?
                }
                None => match candidates.len() {
                    1 => candidates.into_iter().next().expect("one candidate"),
                    0 => return Err(ToolError::new(
                        ErrorCode::DocumentNotOpen,
                        "no board is open in that KiCad; open it in the PCB Editor and press KiCad AI: connect there",
                    )),
                    _ => {
                        return Err(ToolError::new(
                            ErrorCode::InvalidArgument,
                            "several boards are open; pass pcb_path to choose one",
                        ))
                    }
                },
            };
        let cli = installs
            .iter()
            .filter(|i| i.version.full == registration.kicad_version)
            .max_by_key(|i| (i.version.major, i.version.minor, i.version.patch))
            .cloned();
        let render_cli = installs
            .iter()
            .filter(|i| i.version.series() >= 11)
            .max_by_key(|i| (i.version.major, i.version.minor, i.version.patch))
            .cloned();
        Ok(Self {
            id,
            provider: Provider::Gui,
            kicad_version: registration.kicad_version.clone(),
            document_path,
            cli,
            render_cli,
            ipc: Some(Mutex::new(Ipc {
                client,
                document,
                server: None,
            })),
        })
    }

    /// Reads the board file directly (KiCad 10 or 11, no KiCad running): reading, DRC, and exports.
    pub async fn open_file(installs: &[KicadInstall], path: &Path) -> Result<Self, ToolError> {
        let path = check_board_path(path)?;
        let cli = installs
            .iter()
            .max_by_key(|i| (i.version.major, i.version.minor, i.version.patch))
            .cloned()
            .ok_or_else(|| ToolError::new(ErrorCode::KicadNotFound, "no KiCad install with kicad-cli was found"))?;
        let render_cli = installs
            .iter()
            .filter(|i| i.version.series() >= 11)
            .max_by_key(|i| (i.version.major, i.version.minor, i.version.patch))
            .cloned();
        Ok(Self {
            id: new_session_id(),
            provider: Provider::File,
            kicad_version: cli.version.full.clone(),
            document_path: path,
            cli: Some(cli),
            render_cli,
            ipc: None,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn provider(&self) -> Provider {
        self.provider
    }

    pub fn document_path(&self) -> &Path {
        &self.document_path
    }

    pub fn kicad_version(&self) -> &str {
        &self.kicad_version
    }

    pub fn cli(&self) -> Option<&KicadInstall> {
        self.cli.as_ref()
    }

    /// A KiCad 11-series CLI, which 3D renders need.
    pub fn render_cli(&self) -> Option<&KicadInstall> {
        self.render_cli.as_ref()
    }

    /// Does this session talk to a running KiCad (so it can read and change the live board)?
    pub fn is_live(&self) -> bool {
        self.ipc.is_some()
    }

    /// One request to KiCad as it is, for probes and tests.
    #[doc(hidden)]
    pub async fn raw_call<Req: prost::Name, Resp: prost::Name + Default>(
        &self,
        request: &Req,
    ) -> Result<Resp, kicad_ipc::Error> {
        let ipc = self.ipc.as_ref().expect("raw_call needs a live board session");
        ipc.lock().await.client.call(request).await
    }

    /// One request whose answer is empty or not needed, for probes and tests.
    #[doc(hidden)]
    pub async fn raw_call_any<Req: prost::Name>(
        &self,
        request: &Req,
    ) -> Result<Option<prost_types::Any>, kicad_ipc::Error> {
        let ipc = self.ipc.as_ref().expect("raw_call_any needs a live board session");
        ipc.lock().await.client.call_any(request).await
    }

    /// The document identifier KiCad assigned (live sessions).
    #[doc(hidden)]
    pub async fn document_specifier(&self) -> Option<DocumentSpecifier> {
        Some(self.ipc.as_ref()?.lock().await.document.clone())
    }

    pub async fn status(&self) -> BoardStatus {
        let mut can = vec!["read the board", "run DRC", "export through kicad-cli"];
        let (mut connected, mut unsaved, mut pid) = (true, None, None);
        if let Some(ipc) = &self.ipc {
            let mut ipc = ipc.lock().await;
            let running = ipc.server.as_mut().is_none_or(|s| s.is_running());
            connected = running && ipc.client.call_any(&Ping {}).await.is_ok();
            pid = ipc.server.as_ref().and_then(|s| s.pid());
            if connected {
                let document = ipc.document.clone();
                let state: Result<GetDocumentModifiedStateResponse, _> = ipc
                    .client
                    .call(&GetDocumentModifiedState {
                        document: Some(document),
                    })
                    .await;
                unsaved = state.ok().and_then(|r| match DocumentModifiedState::try_from(r.state) {
                    Ok(DocumentModifiedState::DmsModified) => Some(true),
                    Ok(DocumentModifiedState::DmsUnmodified) => Some(false),
                    _ => None,
                });
            }
            can.push("update the board from the schematic");
        }
        BoardStatus {
            session_id: self.id.clone(),
            provider: self.provider,
            kicad_version: self.kicad_version.clone(),
            document: self.document_path.clone(),
            connected,
            unsaved_changes: unsaved,
            server_pid: pid,
            can,
        }
    }

    /// Saves the live board to its file.
    pub async fn save(&self) -> Result<(), ToolError> {
        let ipc = self
            .ipc
            .as_ref()
            .ok_or_else(|| ToolError::new(ErrorCode::FeatureUnavailable, "a file session has nothing to save"))?;
        let mut ipc = ipc.lock().await;
        let document = ipc.document.clone();
        ipc.client
            .call_any(&SaveDocument {
                document: Some(document),
            })
            .await
            .map_err(|e| from_ipc_error(&e, true))?;
        Ok(())
    }

    /// A copy of the board as it is now (with its project) in `dir`, which must exist, for tools
    /// that read files. A file session copies the file.
    pub async fn save_copy(&self, dir: &Path) -> Result<PathBuf, ToolError> {
        let name = self
            .document_path
            .file_name()
            .map(|n| n.to_owned())
            .unwrap_or_else(|| "copy.kicad_pcb".into());
        let target = dir.join(name);
        match &self.ipc {
            Some(ipc) => {
                let mut ipc = ipc.lock().await;
                let document = ipc.document.clone();
                let request = SaveCopyOfDocument {
                    document: Some(document),
                    path: target.display().to_string(),
                    options: None,
                };
                // Writing a copy changes nothing, so a slow answer (a busy machine, a first save
                // that loads libraries) is asked again once.
                let first = ipc.client.call_any(&request).await;
                match first {
                    Err(kicad_ipc::Error::Timeout { .. }) => {
                        ipc.client
                            .call_any(&request)
                            .await
                            .map_err(|e| from_ipc_error(&e, false))?;
                    }
                    other => {
                        other.map_err(|e| from_ipc_error(&e, true))?;
                    }
                }
            }
            None => {
                std::fs::copy(&self.document_path, &target)
                    .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("cannot copy the board: {e}")))?;
            }
        }
        // A project file next to the board lets the CLI use the board's rules.
        if let Some(folder) = self.document_path.parent() {
            if let Some(stem) = self.document_path.file_stem() {
                let pro = folder.join(format!("{}.kicad_pro", stem.to_string_lossy()));
                if pro.is_file() {
                    let _ = std::fs::copy(&pro, dir.join(format!("{}.kicad_pro", stem.to_string_lossy())));
                }
            }
        }
        Ok(target)
    }

    /// Imports a netlist exported from the schematic (matching parts by reference). With `dry_run`
    /// the board is not changed and KiCad reports what would change.
    pub async fn import_netlist(&self, netlist: &Path, dry_run: bool) -> Result<ImportReport, ToolError> {
        let ipc = self.ipc.as_ref().ok_or_else(|| {
            ToolError::new(
                ErrorCode::FeatureUnavailable,
                "updating a board from the schematic needs a live KiCad 11 session (headless or gui)",
            )
        })?;
        let mut ipc = ipc.lock().await;
        // A headless server loads libraries on request, and keeps loading them in the background
        // while it answers; only an import needs the footprint libraries, so they are asked for
        // here and not when the board is opened (reads and DRC would otherwise queue behind the load).
        if self.provider == Provider::Headless {
            let _ = ipc
                .client
                .call_any(&LoadAllLibraries {
                    r#type: vec![LibraryType::LtFootprint as i32],
                })
                .await;
        }
        let board = ipc.document.clone();
        let response: ImportNetlistResponse = ipc
            .client
            .call(&ImportNetlist {
                board: Some(board),
                netlist_path: netlist.display().to_string(),
                dry_run,
                match_mode: NetlistMatchMode::NmmReference as i32,
                delete_extra_footprints: false,
                update_footprints: false,
                transfer_groups: false,
                override_locks: false,
            })
            .await
            .map_err(|e| from_ipc_error(&e, !dry_run))?;
        Ok(ImportReport {
            errors: response.error_count,
            warnings: response.warning_count,
            new_footprints: response.new_footprint_count,
            report: response.report,
        })
    }

    /// Ends the session. A headless session stops its server (refusing with unsaved changes unless
    /// `discard_unsaved`); a GUI session only disconnects.
    pub async fn close(&self, discard_unsaved: bool) -> Result<bool, ToolError> {
        let Some(ipc) = &self.ipc else {
            return Ok(false);
        };
        let mut ipc = ipc.lock().await;
        if ipc.server.is_none() {
            return Ok(false);
        }
        let document = ipc.document.clone();
        let state: Result<GetDocumentModifiedStateResponse, _> = ipc
            .client
            .call(&GetDocumentModifiedState {
                document: Some(document.clone()),
            })
            .await;
        let unsaved = state
            .ok()
            .is_some_and(|r| r.state == DocumentModifiedState::DmsModified as i32);
        if unsaved && !discard_unsaved {
            return Err(ToolError::new(
                ErrorCode::InvalidArgument,
                "the board has unsaved changes; save it first (pcb_save) or pass discard_unsaved: true",
            ));
        }
        let _ = ipc
            .client
            .call_any(&CloseDocument {
                document: Some(document),
            })
            .await;
        if let Some(server) = ipc.server.as_mut() {
            server.stop().await;
        }
        Ok(unsaved)
    }
}

// ---------------------------------------------------------------- reading

use kicad_domain::board::{Board, BoardEdge, BoardFootprint, BoardPad, BoardTrack, BoardVia, BoardZone};
use kicad_domain::{Nm, Point, Rect};
use kicad_ipc::proto::kiapi::board::commands::{
    BoardBoundingBoxMode, BoardBoundingBoxResponse, BoardEnabledLayersResponse, GetBoardBoundingBox,
    GetBoardEnabledLayers, GetNets, NetsResponse,
};
use kicad_ipc::proto::kiapi::board::types::{
    Arc as ArcTrack, BoardGraphicShape, BoardLayer, FootprintInstance, Net, Pad, Track, Via, Zone,
};
use kicad_ipc::proto::kiapi::common::commands::{GetItems, GetItemsResponse};
use kicad_ipc::proto::kiapi::common::types::{ItemHeader, KiCadObjectType, Vector2};
use prost::Name;

/// `BL_F_Cu` is `F.Cu`, `BL_Edge_Cuts` is `Edge.Cuts`.
fn layer_name(value: i32) -> String {
    BoardLayer::try_from(value)
        .map(|l| l.as_str_name().trim_start_matches("BL_").replace('_', "."))
        .unwrap_or_default()
}

fn at(v: &Option<Vector2>) -> Point {
    v.as_ref()
        .map(|v| Point::new(Nm(v.x_nm), Nm(v.y_nm)))
        .unwrap_or_default()
}

fn net_name(n: &Option<Net>) -> String {
    n.as_ref().map(|n| n.name.clone()).unwrap_or_default()
}

fn field_text(f: &Option<kicad_ipc::proto::kiapi::board::types::Field>) -> String {
    f.as_ref()
        .and_then(|f| f.text.as_ref())
        .and_then(|t| t.text.as_ref())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

impl BoardSession {
    /// The board as it is now: from the live KiCad, or from the file.
    pub async fn read(&self) -> Result<Board, ToolError> {
        let Some(ipc) = &self.ipc else {
            let text = std::fs::read_to_string(&self.document_path).map_err(|e| {
                ToolError::new(
                    ErrorCode::KicadError,
                    format!("cannot read {}: {e}", self.document_path.display()),
                )
            })?;
            return kicad_sexpr::pcb::read_board(&text)
                .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("cannot read the board file: {e}")));
        };
        let mut ipc = ipc.lock().await;
        let document = ipc.document.clone();
        let header = Some(ItemHeader {
            document: Some(document.clone()),
            ..Default::default()
        });
        let items: GetItemsResponse = ipc
            .client
            .call(&GetItems {
                header,
                types: [
                    KiCadObjectType::KotPcbFootprint,
                    KiCadObjectType::KotPcbTrace,
                    KiCadObjectType::KotPcbArc,
                    KiCadObjectType::KotPcbVia,
                    KiCadObjectType::KotPcbZone,
                    KiCadObjectType::KotPcbShape,
                    KiCadObjectType::KotPcbText,
                ]
                .map(|t| t as i32)
                .to_vec(),
            })
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        let layers: BoardEnabledLayersResponse = ipc
            .client
            .call(&GetBoardEnabledLayers {
                board: Some(document.clone()),
            })
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        let nets: NetsResponse = ipc
            .client
            .call(&GetNets {
                board: Some(document.clone()),
                netclass_filter: vec![],
            })
            .await
            .map_err(|e| from_ipc_error(&e, false))?;
        let edges: Result<BoardBoundingBoxResponse, _> = ipc
            .client
            .call(&GetBoardBoundingBox {
                board: Some(document),
                mode: BoardBoundingBoxMode::BbbmBoardEdgesOnly as i32,
            })
            .await;
        drop(ipc);

        let mut board = Board {
            copper_layers: layers
                .layers
                .iter()
                .map(|l| layer_name(*l))
                .filter(|n| n.ends_with(".Cu"))
                .collect(),
            nets: nets
                .nets
                .iter()
                .map(|n| n.name.clone())
                .filter(|n| !n.is_empty())
                .collect(),
            ..Default::default()
        };
        // An empty board answers with a zero-size box.
        if let Ok(r) = edges {
            if let Some(b) = r.r#box {
                let (p, s) = (at(&b.position), at(&b.size));
                if s.x.0 > 0 || s.y.0 > 0 {
                    board.outline = Rect::from_points([p, p + s]);
                }
            }
        }
        let copper_names = board.copper_layers.clone();
        let decode_err = |what: &str, e: prost::DecodeError| {
            ToolError::new(ErrorCode::KicadError, format!("undecodable {what}: {e}"))
        };
        for any in &items.items {
            let url = any.type_url.as_str();
            if url == FootprintInstance::type_url() {
                let f: FootprintInstance = any.to_msg().map_err(|e| decode_err("footprint", e))?;
                let lib = f
                    .definition
                    .as_ref()
                    .and_then(|d| d.id.clone())
                    .map(|l| format!("{}:{}", l.library_nickname, l.entry_name))
                    .unwrap_or_default();
                let pads = f
                    .definition
                    .iter()
                    .flat_map(|d| d.items.iter())
                    .filter(|i| i.type_url == Pad::type_url())
                    .filter_map(|i| i.to_msg::<Pad>().ok())
                    .map(|p| BoardPad {
                        kind: match p.r#type {
                            1 => "thru_hole",
                            2 => "smd",
                            3 => "connect",
                            4 => "np_thru_hole",
                            _ => "smd",
                        }
                        .to_owned(),
                        // Pad positions come back already placed on the board.
                        at: at(&p.position),
                        net: net_name(&p.net),
                        size: p
                            .pad_stack
                            .as_ref()
                            .and_then(|s| s.copper_layers.first())
                            .and_then(|l| l.size)
                            .map(|v| (Nm(v.x_nm), Nm(v.y_nm))),
                        angle_deg: p
                            .pad_stack
                            .as_ref()
                            .and_then(|s| s.angle)
                            .map(|a| a.value_degrees)
                            .unwrap_or(0.0),
                        layers: {
                            let named: Vec<String> = p
                                .pad_stack
                                .iter()
                                .flat_map(|s| s.layers.iter())
                                .map(|l| layer_name(*l))
                                .filter(|n| n.ends_with(".Cu"))
                                .collect();
                            // A hole through the board reaches every copper layer.
                            if p.r#type == 1 {
                                copper_names.clone()
                            } else {
                                named
                            }
                        },
                        number: p.number,
                    })
                    .collect();
                board.footprints.push(BoardFootprint {
                    id: f.id.map(|k| k.value).unwrap_or_default(),
                    reference: field_text(&f.reference_field),
                    value: field_text(&f.value_field),
                    lib_id: lib,
                    layer: layer_name(f.layer),
                    position: at(&f.position),
                    rotation_deg: f.orientation.map(|a| a.value_degrees).unwrap_or(0.0),
                    locked: f.locked == 2,
                    dnp: f.attributes.as_ref().is_some_and(|a| a.do_not_populate),
                    exclude_from_bom: f.attributes.as_ref().is_some_and(|a| a.exclude_from_bill_of_materials),
                    exclude_from_pos: f.attributes.as_ref().is_some_and(|a| a.exclude_from_position_files),
                    board_only: f.attributes.as_ref().is_some_and(|a| a.not_in_schematic),
                    pads,
                    courtyard: None,
                    courtyard_local: None,
                });
            } else if url == Track::type_url() {
                let t: Track = any.to_msg().map_err(|e| decode_err("track", e))?;
                board.tracks.push(BoardTrack {
                    id: t.id.map(|k| k.value).unwrap_or_default(),
                    start: at(&t.start),
                    end: at(&t.end),
                    width: Nm(t.width.map(|d| d.value_nm).unwrap_or(0)),
                    layer: layer_name(t.layer),
                    net: net_name(&t.net),
                    locked: t.locked == 2,
                });
            } else if url == ArcTrack::type_url() {
                let t: ArcTrack = any.to_msg().map_err(|e| decode_err("arc track", e))?;
                board.tracks.push(BoardTrack {
                    id: t.id.map(|k| k.value).unwrap_or_default(),
                    start: at(&t.start),
                    end: at(&t.end),
                    width: Nm(t.width.map(|d| d.value_nm).unwrap_or(0)),
                    layer: layer_name(t.layer),
                    net: net_name(&t.net),
                    locked: t.locked == 2,
                });
            } else if url == Via::type_url() {
                let v: Via = any.to_msg().map_err(|e| decode_err("via", e))?;
                let stack = v.pad_stack.unwrap_or_default();
                board.vias.push(BoardVia {
                    id: v.id.map(|k| k.value).unwrap_or_default(),
                    at: at(&v.position),
                    diameter: Nm(stack
                        .copper_layers
                        .first()
                        .and_then(|l| l.size.as_ref())
                        .map(|s| s.x_nm)
                        .unwrap_or(0)),
                    drill: Nm(stack.drill.and_then(|d| d.diameter).map(|d| d.x_nm).unwrap_or(0)),
                    net: net_name(&v.net),
                    locked: v.locked == 2,
                });
            } else if url == Zone::type_url() {
                let z: Zone = any.to_msg().map_err(|e| decode_err("zone", e))?;
                let net = match &z.settings {
                    Some(kicad_ipc::proto::kiapi::board::types::zone::Settings::CopperSettings(c)) => net_name(&c.net),
                    _ => String::new(),
                };
                board.zones.push(BoardZone {
                    id: z.id.map(|k| k.value).unwrap_or_default(),
                    name: z.name,
                    layers: z.layers.iter().map(|l| layer_name(*l)).collect(),
                    net,
                    priority: z.priority,
                    bounds: Rect::from_points(
                        z.outline
                            .iter()
                            .flat_map(|o| o.polygons.iter())
                            .filter_map(|p| p.outline.as_ref())
                            .flat_map(|l| l.nodes.iter())
                            .filter_map(|n| match &n.geometry {
                                Some(kicad_ipc::proto::kiapi::common::types::poly_line_node::Geometry::Point(v)) => {
                                    Some(Point::new(Nm(v.x_nm), Nm(v.y_nm)))
                                }
                                _ => None,
                            }),
                    ),
                    locked: z.locked == 2,
                    filled: z.filled,
                });
            } else {
                board.drawings += 1;
                if url == BoardGraphicShape::type_url() {
                    let s: BoardGraphicShape = any.to_msg().map_err(|e| decode_err("drawing", e))?;
                    if s.layer == BoardLayer::BlEdgeCuts as i32 {
                        let (kind, bounds) = s.shape.as_ref().map(edit::shape_bounds).unwrap_or(("other", None));
                        board.edges.push(BoardEdge {
                            id: s.id.map(|k| k.value).unwrap_or_default(),
                            kind: kind.to_owned(),
                            bounds,
                            locked: s.locked == 2,
                        });
                    }
                }
            }
        }
        Ok(board)
    }
}
