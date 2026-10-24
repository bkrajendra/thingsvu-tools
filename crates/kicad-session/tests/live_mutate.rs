// SPDX-License-Identifier: GPL-3.0-or-later
//! Phase 3 mutation path against the installed nightly (headless). Skipped unless `KICAD_LIVE=1`.

use std::path::PathBuf;
use std::time::Duration;

use kicad_domain::{plan_circuit, CircuitSpec, ErrorCode, LibraryId, MutationState, Operation, OperationPlan, Point};
use kicad_session::{OpenTarget, Session, SessionConfig};

const SPEC: &str = include_str!("../../../docs/examples/two-resistor-series-chain.json");

fn live() -> bool {
    std::env::var("KICAD_LIVE").as_deref() == Ok("1")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("thingsvu-mutate-live-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn new_session(name: &str) -> (Session, PathBuf) {
    let path = scratch(name).join("demo.kicad_sch");
    let installs = kicad_cli_runner::discover(&[]).await;
    let session = Session::open_headless(&SessionConfig::default(), &installs, OpenTarget::New(path.clone()))
        .await
        .unwrap();
    (session, path)
}

async fn plan_for(session: &Session, spec: &CircuitSpec) -> OperationPlan {
    let ids = kicad_domain::library_ids_for(spec);
    let library = session.library_symbols(&ids).await.unwrap();
    plan_circuit(spec, &library, &session.existing_document().await.unwrap())
}

#[tokio::test]
async fn plan_apply_verify_save() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let (session, path) = new_session("demo").await;
    let spec = CircuitSpec::from_json(SPEC).unwrap();
    let plan = plan_for(&session, &spec).await;
    assert!(
        !kicad_domain::validation::has_errors(&plan.findings),
        "{:?}",
        plan.findings
    );

    let fingerprint = session.fingerprint().await.unwrap();
    let report = session.apply_plan(&plan, &fingerprint).await.unwrap();
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    assert!(report.verified, "{:?}", report.verification);
    assert_eq!(report.placed.len(), 2);
    assert_eq!(report.undo_steps, 2, "placement, then values and connections");

    let symbols = session.list_symbols().await.unwrap();
    assert_eq!(
        symbols
            .iter()
            .map(|s| (s.reference.as_str(), s.value.as_str(), s.library_id.as_str()))
            .collect::<Vec<_>>(),
        [("R1", "10k", "Device:R"), ("R2", "10k", "Device:R")]
    );

    // Re-applying the same plan against the changed document is refused.
    let stale = session.apply_plan(&plan, &fingerprint).await.unwrap_err();
    assert_eq!(stale.code, ErrorCode::StalePlan);

    session.save().await.unwrap();
    assert_eq!(session.status().await.unsaved_changes, Some(false));
    let file = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        file.lines().filter(|l| l.starts_with("\t\t(symbol \"")).count(),
        1,
        "one library symbol"
    );

    // Independent check of the saved file with kicad-cli.
    let installs = kicad_cli_runner::discover(&[]).await;
    let cli = installs.iter().find(|i| i.version.series() >= 11).unwrap();
    let netlist =
        kicad_cli_runner::netlist::export_netlist(cli, &path, &path.with_extension("net"), Duration::from_secs(120))
            .await
            .unwrap();
    let mid = netlist.net("/MID").unwrap();
    let mut pins: Vec<_> = mid.nodes.iter().map(|n| format!("{}.{}", n.reference, n.pin)).collect();
    pins.sort();
    assert_eq!(pins, ["R1.2", "R2.1"]);
    session.close(false).await.unwrap();
}

#[tokio::test]
async fn failed_headless_apply_removes_what_it_created() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let (session, _) = new_session("cleanup").await;
    let spec = CircuitSpec::from_json(SPEC).unwrap();
    let mut plan = plan_for(&session, &spec).await;
    // Make the second placement fail after the first succeeded.
    if let Operation::PlaceSymbol {
        library_id, position, ..
    } = &mut plan.operations[1]
    {
        *library_id = LibraryId::parse("Device:ThingsvuDoesNotExist").unwrap();
        *position = Point::from_mm(101.6, 50.8).unwrap();
    }
    let fingerprint = session.fingerprint().await.unwrap();
    let err = session.apply_plan(&plan, &fingerprint).await.unwrap_err();
    println!("{err:?}");
    assert_eq!(err.code, ErrorCode::SymbolNotFound);
    assert_eq!(err.mutation_state, MutationState::NotApplied);
    assert!(
        session.list_symbols().await.unwrap().is_empty(),
        "the first resistor must have been removed"
    );
    assert_eq!(
        session.fingerprint().await.unwrap(),
        fingerprint,
        "document back to its starting state"
    );
    session.close(true).await.unwrap();
}

/// Experiment: which earlier call makes a masked value update right after placement fail.
#[tokio::test]
async fn update_after_prior_calls() {
    if !live() || std::env::var("THINGSVU_EXPERIMENT").is_err() {
        eprintln!("skipped: set KICAD_LIVE=1 and THINGSVU_EXPERIMENT=1");
        return;
    }
    use kicad_ipc::proto::kiapi::common::commands::{PlaceFromLibraryResponse, UpdateItems, UpdateItemsResponse};
    use kicad_ipc::proto::kiapi::common::types::{ItemHeader, LibraryIdentifier, Vector2};
    use kicad_ipc::proto::kiapi::schematic::commands::PlaceSymbolFromLibrary;
    use kicad_ipc::proto::kiapi::schematic::types::SchematicSymbolInstance;
    for variant in ["none", "library", "list_symbols", "netlist", "all"] {
        let (session, _) = new_session(&format!("exp-{variant}")).await;
        let r = LibraryId::parse("Device:R").unwrap();
        if matches!(variant, "library" | "all") {
            session.library_symbols(std::slice::from_ref(&r)).await.unwrap();
        }
        if matches!(variant, "list_symbols" | "all") {
            session.list_symbols().await.unwrap();
        }
        if matches!(variant, "netlist" | "all") {
            session.netlist().await.unwrap();
        }
        let doc = session.document_specifier().clone();
        let placed: PlaceFromLibraryResponse = loop {
            let res = session
                .raw_client()
                .await
                .call(&PlaceSymbolFromLibrary {
                    header: Some(ItemHeader {
                        document: Some(doc.clone()),
                        ..Default::default()
                    }),
                    lib_id: Some(LibraryIdentifier {
                        library_nickname: "Device".into(),
                        entry_name: "R".into(),
                    }),
                    position: Some(Vector2 {
                        x_nm: 50_800_000,
                        y_nm: 50_800_000,
                    }),
                    reference: Some("R1".into()),
                    ..Default::default()
                })
                .await;
            match res {
                Ok(p) => break p,
                Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        };
        let mut symbol = placed.item.unwrap().to_msg::<SchematicSymbolInstance>().unwrap();
        symbol.definition = None;
        if let Some(t) = symbol.value_field.as_mut().and_then(|f| f.text.as_mut()) {
            t.text = "10k".into();
        }
        let res = session
            .raw_client()
            .await
            .call::<_, UpdateItemsResponse>(&UpdateItems {
                header: Some(ItemHeader {
                    document: Some(doc.clone()),
                    field_mask: Some(prost_types::FieldMask {
                        paths: vec!["value_field".into()],
                    }),
                    ..Default::default()
                }),
                items: vec![prost_types::Any::from_msg(&symbol).unwrap()],
            })
            .await
            .unwrap();
        let status = res.updated_items.first().and_then(|i| i.status.clone());
        println!("VARIANT {variant}: {status:?}");
        session.close(true).await.unwrap();
    }
}

/// Experiment: apply_plan with a one-symbol plan.
#[tokio::test]
async fn apply_single_symbol() {
    if !live() || std::env::var("THINGSVU_EXPERIMENT").is_err() {
        eprintln!("skipped: set KICAD_LIVE=1 and THINGSVU_EXPERIMENT=1");
        return;
    }
    let (session, _) = new_session("single").await;
    let spec = CircuitSpec::from_json(SPEC).unwrap();
    let mut plan = plan_for(&session, &spec).await;
    plan.operations.truncate(1);
    plan.expected_nets.clear();
    println!("OP {:?}", plan.operations[0]);
    let fp = session.fingerprint().await.unwrap();
    println!("RESULT {:?}", session.apply_plan(&plan, &fp).await.map(|r| r.placed));
    session.close(true).await.unwrap();
}

#[tokio::test]
async fn show_commit_capability() {
    if !live() || std::env::var("THINGSVU_EXPERIMENT").is_err() {
        return;
    }
    let (session, _) = new_session("caps").await;
    println!(
        "COMMITS {:?}",
        session.capabilities().get(kicad_domain::Capability::Commits)
    );
    session.close(true).await.unwrap();
}
