// SPDX-License-Identifier: GPL-3.0-or-later
//! MCP tools. Phase 1: session management only.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use kicad_cli_runner::KicadInstall;
use kicad_domain::{ErrorCode, Origin, Provenance, ToolError};
use kicad_session::{OpenTarget, Session, SessionConfig};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OnceCell};

use crate::envelope;

const INSTRUCTIONS: &str = "KiCad schematic tools. Start with session_status to see the KiCad installs \
and what each session can do, then session_open on a .kicad_sch file (or create: true for a new one). \
Every result has ok, result_state, and provenance; a check that could not run is reported as not_run, \
never as passing. Close sessions with session_close when done.";

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Explicitly configured kicad-cli paths, tried before automatic discovery.
    pub configured_cli: Vec<PathBuf>,
    pub session: SessionConfig,
}

struct State {
    options: Options,
    installs: OnceCell<Vec<KicadInstall>>,
    sessions: Mutex<BTreeMap<String, Arc<Session>>>,
}

#[derive(Clone)]
pub struct KicadAiServer {
    state: Arc<State>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusArgs {
    /// Report one session. Omit to list every session and the KiCad installs found.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderArg {
    /// A KiCad API server started and owned by this MCP server (KiCad 11 / 10.99).
    #[default]
    Headless,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenArgs {
    /// Absolute path of the .kicad_sch file.
    pub schematic_path: String,
    /// Create a new schematic at schematic_path instead of opening an existing one.
    #[serde(default)]
    pub create: bool,
    #[serde(default)]
    pub provider: ProviderArg,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloseArgs {
    pub session_id: String,
    /// Close even if the document has unsaved changes (they are lost).
    #[serde(default)]
    pub discard_unsaved: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionArgs {
    pub session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SymbolPinsArgs {
    pub session_id: String,
    /// Symbol id (UUID from sch_list_symbols), or a reference such as R1 that matches exactly
    /// one symbol. Prefer the id: references can change when a schematic is re-annotated.
    pub symbol: String,
}

#[derive(Serialize)]
struct InstallInfo<'a> {
    cli: String,
    version: &'a str,
    series: u32,
    development_build: bool,
    found_via: kicad_cli_runner::InstallSource,
}

fn provenance(status: &kicad_session::SessionStatus) -> Provenance {
    Provenance {
        origin: Origin::KicadIpc,
        kicad_version: Some(status.kicad_version.clone()),
        provider: Some("headless".into()),
        document: status.document.as_ref().map(|d| d.display().to_string()),
        revision: None,
    }
}

/// Provenance for data read live from a session's KiCad.
fn live(session: &Session) -> Vec<Provenance> {
    vec![Provenance {
        origin: Origin::KicadIpc,
        kicad_version: Some(session.kicad_version().to_owned()),
        provider: Some("headless".into()),
        document: Some(session.document_path().display().to_string()),
        revision: None,
    }]
}

impl KicadAiServer {
    pub fn new(options: Options) -> Self {
        Self {
            state: Arc::new(State {
                options,
                installs: OnceCell::new(),
                sessions: Mutex::new(BTreeMap::new()),
            }),
            tool_router: Self::tool_router(),
        }
    }

    async fn installs(&self) -> &[KicadInstall] {
        self.state
            .installs
            .get_or_init(|| async {
                let found = kicad_cli_runner::discover(&self.state.options.configured_cli).await;
                tracing::info!(count = found.len(), "KiCad installs discovered");
                found
            })
            .await
    }

    async fn session(&self, id: &str) -> Result<Arc<Session>, ToolError> {
        self.state.sessions.lock().await.get(id).cloned().ok_or_else(|| {
            ToolError::new(
                ErrorCode::NoSession,
                format!("no session '{id}'; call session_status to list sessions"),
            )
        })
    }
}

#[tool_router(router = tool_router)]
impl KicadAiServer {
    #[tool(
        name = "session_status",
        description = "List KiCad installs and open sessions, or report one session: KiCad version, document, \
liveness, unsaved changes, and the capability matrix (which features are available and through which backend)."
    )]
    pub async fn session_status(&self, Parameters(args): Parameters<StatusArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        if let Some(id) = args.session_id {
            return match self.session(&id).await {
                Ok(session) => {
                    let status = session.status().await;
                    let prov = provenance(&status);
                    envelope::ok(&cid, status, vec![prov], vec![])
                }
                Err(e) => envelope::error(&cid, &e),
            };
        }

        let installs: Vec<_> = self
            .installs()
            .await
            .iter()
            .map(|i| InstallInfo {
                cli: i.cli.display().to_string(),
                version: &i.version.full,
                series: i.version.series(),
                development_build: i.version.is_development_build(),
                found_via: i.source,
            })
            .collect();
        let sessions: Vec<_> = self.state.sessions.lock().await.values().cloned().collect();
        let mut statuses = Vec::new();
        for s in sessions {
            statuses.push(s.status().await);
        }
        let warnings = if installs.iter().any(|i| i.series >= 11) {
            vec![]
        } else {
            vec!["no KiCad 11 (or 10.99 nightly) install found: headless sessions are unavailable".to_owned()]
        };
        envelope::ok(
            &cid,
            serde_json::json!({
                "server": { "name": env!("CARGO_PKG_NAME"), "version": env!("CARGO_PKG_VERSION") },
                "kicad_installs": installs,
                "sessions": statuses,
            }),
            vec![],
            warnings,
        )
    }

    #[tool(
        name = "session_open",
        description = "Open a .kicad_sch file (or create a new one with create: true) in a new session. The \
headless provider starts a private KiCad 11 API server owned by this MCP server. Refuses a file that a KiCad \
editor has open. Returns the session status, including its capability matrix."
    )]
    pub async fn session_open(&self, Parameters(args): Parameters<OpenArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let path = PathBuf::from(&args.schematic_path);
        if !path.is_absolute() {
            return envelope::error(
                &cid,
                &ToolError::new(ErrorCode::InvalidArgument, "schematic_path must be an absolute path"),
            );
        }
        let target = if args.create {
            OpenTarget::New(path)
        } else {
            OpenTarget::Existing(path)
        };
        let ProviderArg::Headless = args.provider;
        let installs = self.installs().await.to_vec();
        match Session::open_headless(&self.state.options.session, &installs, target).await {
            Ok(session) => {
                let session = Arc::new(session);
                let status = session.status().await;
                self.state
                    .sessions
                    .lock()
                    .await
                    .insert(session.id().to_owned(), session);
                let prov = provenance(&status);
                envelope::ok(&cid, status, vec![prov], vec![])
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "session_close",
        description = "Close a session and stop its KiCad server. Refuses while the document has unsaved \
changes unless discard_unsaved is true."
    )]
    pub async fn session_close(&self, Parameters(args): Parameters<CloseArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        match session.close(args.discard_unsaved).await {
            Ok(discarded) => {
                self.state.sessions.lock().await.remove(&args.session_id);
                envelope::ok(
                    &cid,
                    serde_json::json!({ "session_id": args.session_id, "closed": true, "discarded_unsaved_changes": discarded }),
                    vec![],
                    vec![],
                )
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "session_list_documents",
        description = "Documents open in the session's KiCad instance (schematics and boards), as KiCad reports them."
    )]
    pub async fn session_list_documents(&self, Parameters(args): Parameters<SessionArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        match self.session(&args.session_id).await {
            Ok(session) => match session.open_documents().await {
                Ok(docs) => envelope::ok(&cid, serde_json::json!({ "documents": docs }), live(&session), vec![]),
                Err(e) => envelope::error(&cid, &e),
            },
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_list_symbols",
        description = "Every placed symbol in the session's schematic: id (stable UUID), reference, value, \
library id, footprint, position in mm, orientation, unit, and sheet path. Read live from KiCad."
    )]
    pub async fn sch_list_symbols(&self, Parameters(args): Parameters<SessionArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        match self.session(&args.session_id).await {
            Ok(session) => match session.list_symbols().await {
                Ok(symbols) => envelope::ok(
                    &cid,
                    serde_json::json!({ "count": symbols.len(), "symbols": symbols }),
                    live(&session),
                    vec![],
                ),
                Err(e) => envelope::error(&cid, &e),
            },
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_get_symbol_pins",
        description = "Pins of one symbol: number, name, electrical type, and connection point on the sheet \
in mm (after the symbol's rotation and mirroring). Wires must end exactly on these points to connect."
    )]
    pub async fn sch_get_symbol_pins(&self, Parameters(args): Parameters<SymbolPinsArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        match self.session(&args.session_id).await {
            Ok(session) => match session.symbol_pins(&args.symbol).await {
                Ok(pins) => envelope::ok(&cid, pins, live(&session), vec![]),
                Err(e) => envelope::error(&cid, &e),
            },
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_get_netlist",
        description = "Connectivity as KiCad's netlister sees it: each net with its symbol pins (reference, \
pin number, symbol id) and a count of other connected items such as wires and labels. Use it to verify that \
intended connections really exist."
    )]
    pub async fn sch_get_netlist(&self, Parameters(args): Parameters<SessionArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        match self.session(&args.session_id).await {
            Ok(session) => match session.netlist().await {
                Ok(nets) => {
                    let unconnected = nets.iter().filter(|n| n.name.starts_with("unconnected-")).count();
                    envelope::ok(
                        &cid,
                        serde_json::json!({ "count": nets.len(), "unconnected_pins": unconnected, "nets": nets }),
                        live(&session),
                        vec![],
                    )
                }
                Err(e) => envelope::error(&cid, &e),
            },
            Err(e) => envelope::error(&cid, &e),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for KicadAiServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
