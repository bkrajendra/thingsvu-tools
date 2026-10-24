// SPDX-License-Identifier: GPL-3.0-or-later
//! MCP tools: sessions, schematic reads, and planned schematic changes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use kicad_cli_runner::KicadInstall;
use kicad_domain::{plan_circuit, CircuitSpec, ErrorCode, LibraryId, Origin, Provenance, ResultState, ToolError};
use kicad_session::registry::{self, Registration};
use kicad_session::{OpenTarget, Provider, Session, SessionConfig};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OnceCell};

use crate::envelope;
use crate::plans::{plan_view, PlanStore, StoredPlan};

const INSTRUCTIONS: &str = "KiCad schematic tools. Start with session_status to see the KiCad installs \
and what each session can do, then session_open on a .kicad_sch file (or create: true for a new one). \
Every result has ok, result_state, and provenance; a check that could not run is reported as not_run, \
never as passing. To change a schematic, describe the circuit as components and nets, call \
sch_plan_changes, show the plan to the user, then call sch_apply_plan with confirm: true and a fresh \
idempotency_key. sch_apply_plan reads connectivity back from KiCad and reports any mismatch. Save with \
sch_save; check with sch_run_erc and sch_render. Close sessions with session_close when done.";

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Explicitly configured kicad-cli paths, tried before automatic discovery.
    pub configured_cli: Vec<PathBuf>,
    pub session: SessionConfig,
    /// Reject every tool that changes a schematic or writes files into a project.
    pub read_only: bool,
}

struct State {
    options: Options,
    installs: OnceCell<Vec<KicadInstall>>,
    sessions: Mutex<BTreeMap<String, Arc<Session>>>,
    plans: Mutex<PlanStore>,
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
    /// The KiCad 11 editor the user has open, after they pressed the "KiCad AI" toolbar button
    /// (kicad-ai-plugin). Changes appear live in the editor and can be undone there.
    Gui,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenArgs {
    /// Absolute path of the .kicad_sch file. Required for headless; for gui, it selects the
    /// project (root schematic) when the editor has more than one open.
    #[serde(default)]
    pub schematic_path: Option<String>,
    /// Headless only: create a new schematic at schematic_path instead of opening an existing one.
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

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanArgs {
    pub session_id: String,
    /// Electrical intent: {"schema_version": 1, "name": "...", "components": [{"id", "library_id"
    /// (e.g. "Device:R"), "value", optional "reference_preference", "footprint", "fields"}], "nets":
    /// [{"name", "pins": [{"component_id", "pin_number"}]}]}. Coordinates are chosen by the layout.
    pub circuit: serde_json::Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyArgs {
    pub session_id: String,
    pub plan_id: String,
    /// Must be true: the user has reviewed the plan.
    pub confirm: bool,
    /// A new value per intended apply (8–128 characters). Repeating a key returns the earlier
    /// result instead of applying twice.
    pub idempotency_key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportArgs {
    pub session_id: String,
    /// Absolute path of the .net file to write, inside the schematic's folder.
    pub output_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenderArgs {
    pub session_id: String,
    /// Resolution, 72–600 (default 150).
    #[serde(default)]
    pub dpi: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DemoArgs {
    pub session_id: String,
    /// Apply the plan as well. Without it, only the plan is returned.
    #[serde(default)]
    pub apply: bool,
    /// Required with apply: true.
    #[serde(default)]
    pub confirm: bool,
    /// Required with apply: true.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// The circuit `sch_create_demo_circuit` builds.
const DEMO_CIRCUIT: &str = include_str!("../../../docs/examples/two-resistor-series-chain.json");

#[derive(Serialize)]
struct InstallInfo<'a> {
    cli: String,
    version: &'a str,
    series: u32,
    development_build: bool,
    found_via: kicad_cli_runner::InstallSource,
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Headless => "headless",
        Provider::Gui => "gui",
    }
}

fn provenance(status: &kicad_session::SessionStatus) -> Provenance {
    Provenance {
        origin: Origin::KicadIpc,
        kicad_version: Some(status.kicad_version.clone()),
        provider: Some(provider_name(status.provider).into()),
        document: status.document.as_ref().map(|d| d.display().to_string()),
        revision: None,
    }
}

/// Provenance for data read live from a session's KiCad.
fn live(session: &Session) -> Vec<Provenance> {
    vec![Provenance {
        origin: Origin::KicadIpc,
        kicad_version: Some(session.kicad_version().to_owned()),
        provider: Some(provider_name(session.provider()).into()),
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
                plans: Mutex::new(PlanStore::default()),
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

    /// Registered KiCad GUIs that still answer. Records of instances that are gone are removed.
    async fn live_registrations(&self) -> Vec<Registration> {
        let dir = registry::registry_dir();
        let mut live = Vec::new();
        for r in registry::list(&dir) {
            if registry::is_reachable(&r, std::time::Duration::from_secs(3)).await {
                live.push(r);
            } else {
                let _ = registry::remove(&dir, &r.socket_path);
            }
        }
        live
    }

    /// The registered GUI to use: the one with `schematic`'s project open, or the only one.
    async fn pick_registration(&self, schematic: Option<&std::path::Path>) -> Result<Registration, ToolError> {
        let live = self.live_registrations().await;
        if live.is_empty() {
            return Err(ToolError::new(
                ErrorCode::NoSession,
                "no KiCad editor is registered: open the project in KiCad 11 and press the \"KiCad AI\" toolbar button",
            ));
        }
        if let Some(schematic) = schematic {
            let folder = schematic.parent().map(|p| p.to_string_lossy().to_lowercase());
            let stem = schematic.file_stem().map(|s| s.to_string_lossy().to_lowercase());
            let matches = |r: &&Registration| {
                r.documents.iter().any(|d| {
                    d.project.as_ref().map(|p| p.to_lowercase()) == stem
                        && d.project_path
                            .as_ref()
                            .map(|p| p.trim_end_matches(['\\', '/']).to_lowercase())
                            == folder
                })
            };
            if let Some(r) = live.iter().find(matches) {
                return Ok(r.clone());
            }
        }
        match live.as_slice() {
            [only] => Ok(only.clone()),
            _ => Err(ToolError::new(
                ErrorCode::InvalidArgument,
                "several KiCad editors are registered; pass schematic_path to choose one",
            )),
        }
    }

    fn refuse_if_read_only(&self) -> Result<(), ToolError> {
        if self.state.options.read_only {
            Err(ToolError::new(
                ErrorCode::ReadOnlyMode,
                "the server runs in read-only mode",
            ))
        } else {
            Ok(())
        }
    }

    /// A private scratch folder for one tool call (ERC copies, renders).
    fn scratch_dir(&self, session_id: &str, purpose: &str, cid: &str) -> Result<PathBuf, ToolError> {
        let dir = self
            .state
            .options
            .session
            .runtime_dir
            .join(session_id)
            .join(format!("{purpose}-{cid}"));
        std::fs::create_dir_all(&dir)
            .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("cannot create {}: {e}", dir.display())))?;
        Ok(dir)
    }

    /// Validates, lays out, and stores a plan; returns its client view.
    async fn make_plan(&self, session: &Arc<Session>, spec: CircuitSpec) -> Result<serde_json::Value, ToolError> {
        let mut ids: Vec<LibraryId> = spec.components.iter().map(|c| c.library_id.clone()).collect();
        ids.sort();
        ids.dedup();
        let library = session.library_symbols(&ids).await?;
        let existing = session.existing_document().await?;
        let plan = plan_circuit(&spec, &library, &existing);
        let fingerprint = session.fingerprint().await?;
        let mut plans = self.state.plans.lock().await;
        let stored = StoredPlan {
            session_id: session.id().to_owned(),
            plan,
            fingerprint,
            applied_with: None,
        };
        let view_plan = stored.plan.clone();
        let id = plans.insert(stored);
        Ok(plan_view(&id, &view_plan))
    }

    /// Applies a stored plan once per idempotency key.
    async fn apply_planned(&self, cid: &str, session: &Arc<Session>, plan_id: &str, key: &str) -> CallToolResult {
        if !(8..=128).contains(&key.len()) {
            return envelope::error(
                cid,
                &ToolError::new(ErrorCode::InvalidArgument, "idempotency_key must be 8–128 characters"),
            );
        }
        let (plan, fingerprint) = {
            let plans = self.state.plans.lock().await;
            if let Some((previous_plan, envelope)) = plans.previous_apply(key) {
                if previous_plan == plan_id {
                    let mut replay = envelope.clone();
                    if let Some(w) = replay.get_mut("warnings").and_then(|w| w.as_array_mut()) {
                        w.push("replayed: this idempotency key was already applied; nothing was changed".into());
                    }
                    return CallToolResult::structured(replay);
                }
                return envelope::error(
                    cid,
                    &ToolError::new(
                        ErrorCode::InvalidArgument,
                        format!("idempotency_key was already used for {previous_plan}"),
                    ),
                );
            }
            let Some(stored) = plans.get(plan_id) else {
                return envelope::error(
                    cid,
                    &ToolError::new(ErrorCode::StalePlan, format!("no plan '{plan_id}'; make a new plan")),
                );
            };
            if stored.session_id != session.id() {
                return envelope::error(
                    cid,
                    &ToolError::new(
                        ErrorCode::InvalidArgument,
                        format!("{plan_id} belongs to another session"),
                    ),
                );
            }
            if let Some(used) = &stored.applied_with {
                return envelope::error(
                    cid,
                    &ToolError::new(
                        ErrorCode::StalePlan,
                        format!("{plan_id} was already applied (key {used}); make a new plan"),
                    ),
                );
            }
            (stored.plan.clone(), stored.fingerprint.clone())
        };

        let result = session.apply_plan(&plan, &fingerprint).await;
        let envelope = match &result {
            Ok(report) => {
                let (state, warnings) = if report.verified {
                    (ResultState::Done, vec![])
                } else {
                    (
                        ResultState::Partial,
                        vec!["applied, but KiCad's connectivity differs from the plan; see verification".to_owned()],
                    )
                };
                envelope::ok_with_state(cid, state, report, live(session), warnings)
            }
            Err(e) => envelope::error(cid, e),
        };
        // A refusal before anything changed may be retried with the same key; anything else is final.
        let final_outcome = match &result {
            Ok(_) => true,
            Err(e) => e.mutation_state != kicad_domain::MutationState::NotStarted,
        };
        if final_outcome {
            let mut plans = self.state.plans.lock().await;
            if let Some(stored) = plans.get_mut(plan_id) {
                stored.applied_with = Some(key.to_owned());
            }
            if let Some(value) = envelope::structured(&envelope) {
                plans.record_apply(key.to_owned(), plan_id.to_owned(), value.clone());
            }
        }
        envelope
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
        // Registered KiCad editors (the token is never shown).
        let gui_editors: Vec<_> = self
            .live_registrations()
            .await
            .iter()
            .map(|r| {
                serde_json::json!({
                    "kicad_version": r.kicad_version,
                    "projects": r.documents.iter().filter(|d| d.kind == "schematic").map(|d| serde_json::json!({ "project": d.project, "folder": d.project_path })).collect::<Vec<_>>(),
                })
            })
            .collect();
        envelope::ok(
            &cid,
            serde_json::json!({
                "server": { "name": env!("CARGO_PKG_NAME"), "version": env!("CARGO_PKG_VERSION") },
                "kicad_installs": installs,
                "gui_editors": gui_editors,
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
        let path = args.schematic_path.as_ref().map(PathBuf::from);
        if path.as_ref().is_some_and(|p| !p.is_absolute()) {
            return envelope::error(
                &cid,
                &ToolError::new(ErrorCode::InvalidArgument, "schematic_path must be an absolute path"),
            );
        }
        let installs = self.installs().await.to_vec();
        let opened = match args.provider {
            ProviderArg::Headless => {
                let Some(path) = path else {
                    return envelope::error(
                        &cid,
                        &ToolError::new(
                            ErrorCode::InvalidArgument,
                            "schematic_path is required for headless sessions",
                        ),
                    );
                };
                let target = if args.create {
                    OpenTarget::New(path)
                } else {
                    OpenTarget::Existing(path)
                };
                Session::open_headless(&self.state.options.session, &installs, target).await
            }
            ProviderArg::Gui => {
                if args.create {
                    return envelope::error(
                        &cid,
                        &ToolError::new(
                            ErrorCode::InvalidArgument,
                            "create is only for headless sessions; create the project in KiCad",
                        ),
                    );
                }
                match self.pick_registration(path.as_deref()).await {
                    Ok(registration) => {
                        Session::open_gui(&self.state.options.session, &installs, &registration, path.as_deref()).await
                    }
                    Err(e) => Err(e),
                }
            }
        };
        match opened {
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
                self.state.plans.lock().await.forget_session(&args.session_id);
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

    #[tool(
        name = "sch_plan_changes",
        description = "Turn a circuit (components and the nets connecting their pins) into an exact plan: \
which symbols are placed where, which references they get, and which wires and labels connect them. \
Nothing is changed. Returns a plan_id for sch_apply_plan, the operations in mm, the nets that must exist \
afterwards, and findings (errors block applying)."
    )]
    pub async fn sch_plan_changes(&self, Parameters(args): Parameters<PlanArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let spec: CircuitSpec = match serde_json::from_value(args.circuit) {
            Ok(spec) => spec,
            Err(e) => {
                return envelope::error(
                    &cid,
                    &ToolError::new(ErrorCode::InvalidArgument, format!("circuit: {e}")),
                )
            }
        };
        match self.make_plan(&session, spec).await {
            Ok(view) => envelope::ok(&cid, view, live(&session), vec![]),
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_apply_plan",
        description = "Apply a plan from sch_plan_changes after the user has reviewed it (confirm: true). \
Refused if the schematic changed since planning. Places symbols, sets values, adds wires and labels, then \
reads connectivity back from KiCad and compares it with the plan's expected nets. On failure every change \
is undone. Repeating the same idempotency_key returns the earlier result."
    )]
    pub async fn sch_apply_plan(&self, Parameters(args): Parameters<ApplyArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        if let Err(e) = self.refuse_if_read_only() {
            return envelope::error(&cid, &e);
        }
        if !args.confirm {
            return envelope::error(
                &cid,
                &ToolError::new(
                    ErrorCode::InvalidArgument,
                    "confirm must be true; show the plan to the user first",
                ),
            );
        }
        match self.session(&args.session_id).await {
            Ok(session) => {
                self.apply_planned(&cid, &session, &args.plan_id, &args.idempotency_key)
                    .await
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(name = "sch_save", description = "Save the session's schematic to its file.")]
    pub async fn sch_save(&self, Parameters(args): Parameters<SessionArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        if let Err(e) = self.refuse_if_read_only() {
            return envelope::error(&cid, &e);
        }
        match self.session(&args.session_id).await {
            Ok(session) => match session.save().await {
                Ok(()) => envelope::ok(
                    &cid,
                    serde_json::json!({ "saved": session.document_path().display().to_string() }),
                    live(&session),
                    vec![],
                ),
                Err(e) => envelope::error(&cid, &e),
            },
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_run_erc",
        description = "Run KiCad's electrical rules check on the schematic as it is now (unsaved changes \
included). Returns error and warning counts and every violation with its position in mm. Zero errors does \
not prove the circuit works."
    )]
    pub async fn sch_run_erc(&self, Parameters(args): Parameters<SessionArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let Some(cli) = session.cli().cloned() else {
            return envelope::error(
                &cid,
                &ToolError::new(ErrorCode::ErcUnavailable, "no matching kicad-cli install"),
            );
        };
        let dir = match self.scratch_dir(session.id(), "erc", &cid) {
            Ok(d) => d,
            Err(e) => return envelope::error(&cid, &e),
        };
        // ERC runs on a snapshot of the in-memory document, so the user's file is not touched.
        let result = async {
            let copy = session.save_copy(&dir).await?;
            kicad_cli_runner::erc::run_erc(&cli, &copy, &dir.join("erc.json"), std::time::Duration::from_secs(180))
                .await
                .map_err(|e| ToolError::new(ErrorCode::ErcUnavailable, e.to_string()))
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Ok(report) => {
                let violations: Vec<_> = report
                    .violations()
                    .map(|(sheet, v)| {
                        serde_json::json!({
                            "sheet": sheet.path,
                            "severity": v.severity,
                            "type": v.kind,
                            "description": v.description,
                            "items": v.items.iter().map(|i| serde_json::json!({ "description": i.description, "position_mm": i.pos })).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                let provenance = vec![Provenance {
                    origin: Origin::KicadCli,
                    kicad_version: Some(cli.version.full.clone()),
                    provider: None,
                    document: Some(session.document_path().display().to_string()),
                    revision: None,
                }];
                envelope::ok(
                    &cid,
                    serde_json::json!({
                        "errors": report.count("error"),
                        "warnings": report.count("warning"),
                        "violations": violations,
                    }),
                    provenance,
                    vec!["ERC ran on a snapshot of the in-memory schematic".to_owned()],
                )
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_export_netlist",
        description = "Write a KiCad netlist (.net, s-expression format) of the schematic as it is now to \
output_path, which must be inside the schematic's folder. Returns the net and component counts."
    )]
    pub async fn sch_export_netlist(&self, Parameters(args): Parameters<ExportArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        if let Err(e) = self.refuse_if_read_only() {
            return envelope::error(&cid, &e);
        }
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let output = PathBuf::from(&args.output_path);
        let folder = session.document_path().parent().map(PathBuf::from).unwrap_or_default();
        let inside = output.is_absolute()
            && std::path::absolute(&output)
                .is_ok_and(|p| p.parent() == Some(folder.as_path()) || p.starts_with(&folder));
        if !inside || output.extension().and_then(|e| e.to_str()) != Some("net") {
            return envelope::error(
                &cid,
                &ToolError::new(
                    ErrorCode::InvalidArgument,
                    format!("output_path must be an absolute .net path inside {}", folder.display()),
                ),
            );
        }
        let Some(cli) = session.cli().cloned() else {
            return envelope::error(
                &cid,
                &ToolError::new(ErrorCode::FeatureUnavailable, "no matching kicad-cli install"),
            );
        };
        let dir = match self.scratch_dir(session.id(), "netlist", &cid) {
            Ok(d) => d,
            Err(e) => return envelope::error(&cid, &e),
        };
        // KiCad's IPC export jobs read the saved file, so export from a snapshot of the in-memory
        // document instead (docs/tasks.md, 3.7).
        let result = async {
            let copy = session.save_copy(&dir).await?;
            kicad_cli_runner::netlist::export_netlist(&cli, &copy, &output, std::time::Duration::from_secs(180))
                .await
                .map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Ok(netlist) => envelope::ok(
                &cid,
                serde_json::json!({
                    "path": output.display().to_string(),
                    "components": netlist.components.len(),
                    "nets": netlist.nets.len(),
                }),
                live(&session),
                vec![],
            ),
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_render",
        description = "Render the schematic as it is now to a PNG image and return it, so the layout can be \
checked visually."
    )]
    pub async fn sch_render(&self, Parameters(args): Parameters<RenderArgs>) -> CallToolResult {
        use base64::Engine;
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let dpi = args.dpi.unwrap_or(150);
        if !(72..=600).contains(&dpi) {
            return envelope::error(&cid, &ToolError::new(ErrorCode::InvalidArgument, "dpi must be 72–600"));
        }
        let dir = match self.scratch_dir(session.id(), "render", &cid) {
            Ok(d) => d,
            Err(e) => return envelope::error(&cid, &e),
        };
        let Some(cli) = session.cli().cloned() else {
            return envelope::error(
                &cid,
                &ToolError::new(ErrorCode::FeatureUnavailable, "no matching kicad-cli install"),
            );
        };
        // Render a snapshot of the in-memory document (IPC plot jobs read the saved file).
        let result = async {
            let snapshot = dir.join("snapshot");
            let images = dir.join("png");
            for d in [&snapshot, &images] {
                std::fs::create_dir_all(d).map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))?;
            }
            let copy = session.save_copy(&snapshot).await?;
            let pngs =
                kicad_cli_runner::export::export_png(&cli, &copy, &images, dpi, std::time::Duration::from_secs(180))
                    .await
                    .map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))?;
            std::fs::read(&pngs[0]).map_err(|e| ToolError::new(ErrorCode::KicadError, e.to_string()))
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Ok(bytes) => {
                let mut r = envelope::ok(
                    &cid,
                    serde_json::json!({ "format": "png", "dpi": dpi, "bytes": bytes.len() }),
                    live(&session),
                    vec![],
                );
                r.content.push(rmcp::model::ContentBlock::image(
                    base64::engine::general_purpose::STANDARD.encode(&bytes),
                    "image/png",
                ));
                r
            }
            Err(e) => envelope::error(&cid, &e),
        }
    }

    #[tool(
        name = "sch_create_demo_circuit",
        description = "Plan (and with apply: true, confirm: true, and an idempotency_key, also apply) the \
two-resistor series chain INPUT - R1 10k - MID - R2 10k - OUTPUT. A quick end-to-end check of planning, \
applying, and verification."
    )]
    pub async fn sch_create_demo_circuit(&self, Parameters(args): Parameters<DemoArgs>) -> CallToolResult {
        let cid = envelope::correlation_id();
        let session = match self.session(&args.session_id).await {
            Ok(s) => s,
            Err(e) => return envelope::error(&cid, &e),
        };
        let spec = CircuitSpec::from_json(DEMO_CIRCUIT).expect("bundled demo circuit is valid");
        let view = match self.make_plan(&session, spec).await {
            Ok(v) => v,
            Err(e) => return envelope::error(&cid, &e),
        };
        if !args.apply {
            return envelope::ok(
                &cid,
                view,
                live(&session),
                vec!["plan only; pass apply: true to build it".into()],
            );
        }
        if let Err(e) = self.refuse_if_read_only() {
            return envelope::error(&cid, &e);
        }
        let (true, Some(key)) = (args.confirm, args.idempotency_key.as_deref()) else {
            return envelope::error(
                &cid,
                &ToolError::new(
                    ErrorCode::InvalidArgument,
                    "apply: true needs confirm: true and an idempotency_key",
                ),
            );
        };
        let plan_id = view["plan_id"].as_str().unwrap_or_default().to_owned();
        self.apply_planned(&cid, &session, &plan_id, key).await
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
