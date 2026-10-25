// SPDX-License-Identifier: GPL-3.0-or-later
//! Tidying an existing schematic against real KiCad, through the file provider and through the
//! API. Skipped unless `KICAD_LIVE=1`. Renders go to `THINGSVU_LAYOUT_OUT`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kicad_domain::{design_library_ids, library_ids_for, plan_tidy, tidy_spec, OperationPlan, Severity};
use kicad_session::{AnySession, FileSession, OpenTarget, Session, SessionConfig};

fn live() -> bool {
    std::env::var("KICAD_LIVE").as_deref() == Ok("1")
}

fn out_dir() -> PathBuf {
    let dir = std::env::var_os("THINGSVU_LAYOUT_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("thingsvu-layout-review"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A copy of the fixture in a fresh folder.
fn fixture_copy(name: &str) -> PathBuf {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/v11/regulator-with-notes/regulator.kicad_sch");
    let dir = std::env::temp_dir().join(format!("thingsvu-tidy-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("regulator.kicad_sch");
    std::fs::copy(source, &target).unwrap();
    target
}

/// Plans a tidy the way the MCP tool does.
async fn tidy_plan(session: &AnySession) -> (kicad_domain::SchematicDesign, OperationPlan) {
    let design = session.design().await.unwrap();
    let nets = session.netlist().await.unwrap();
    let mut ids = design_library_ids(&design);
    let spec = tidy_spec(&design, &nets).expect("the fixture can be tidied");
    ids.extend(library_ids_for(&spec));
    ids.sort();
    ids.dedup();
    let library = session.library_symbols(&ids).await.unwrap();
    let plan = plan_tidy(&design, &nets, &library);
    (design, plan)
}

fn topology(nets: &[kicad_domain::NetInfo]) -> BTreeSet<(String, BTreeSet<String>)> {
    nets.iter()
        .map(|n| {
            (
                n.name.trim_start_matches('/').to_owned(),
                n.pins
                    .iter()
                    .filter(|p| !p.reference.starts_with('#'))
                    .map(|p| format!("{}.{}", p.reference, p.pin))
                    .collect::<BTreeSet<String>>(),
            )
        })
        .filter(|(_, pins)| !pins.is_empty())
        .collect()
}

async fn render(session: &AnySession, schematic: &Path, label: &str, installs: &[kicad_cli_runner::KicadInstall]) {
    let cli = installs.iter().find(|i| i.version.series() >= 11).unwrap();
    let dir = schematic.parent().unwrap().join(format!("png-{label}"));
    std::fs::create_dir_all(&dir).unwrap();
    let copy = session.save_copy(&dir).await.unwrap();
    let pngs = kicad_cli_runner::export::export_png(cli, &copy, &dir, 110, Duration::from_secs(120))
        .await
        .unwrap();
    let target = out_dir().join(format!("tidy-{label}.png"));
    std::fs::copy(&pngs[0], &target).unwrap();
    println!("RENDER {}", target.display());
}

/// ERC errors in the document as it is now (on a copy).
async fn erc_errors(
    session: &AnySession,
    schematic: &Path,
    label: &str,
    installs: &[kicad_cli_runner::KicadInstall],
) -> usize {
    let cli = installs.iter().find(|i| i.version.series() >= 11).unwrap();
    let dir = schematic.parent().unwrap().join(format!("erc-{label}"));
    std::fs::create_dir_all(&dir).unwrap();
    let copy = session.save_copy(&dir).await.unwrap();
    let report = kicad_cli_runner::erc::run_erc(cli, &copy, &dir.join("erc.json"), Duration::from_secs(120))
        .await
        .unwrap();
    for (_, v) in report.violations() {
        println!("ERC {label}: {} {}", v.kind, v.description);
    }
    report.count("error")
}

/// Review, tidy, check, tidy again: the same story on either kind of session.
async fn tidy_story(
    session: AnySession,
    schematic: PathBuf,
    label: &str,
    installs: Vec<kicad_cli_runner::KicadInstall>,
) {
    let errors_before = erc_errors(&session, &schematic, &format!("{label}-before"), &installs).await;
    let before_nets = topology(&session.netlist().await.unwrap());
    assert!(before_nets.len() >= 6, "{before_nets:?}");
    render(&session, &schematic, &format!("{label}-before"), &installs).await;

    let (design_before, plan) = tidy_plan(&session).await;
    let errors: Vec<_> = plan.findings.iter().filter(|f| f.severity == Severity::Error).collect();
    assert!(errors.is_empty(), "{errors:#?}");
    println!("TIDY {label}: {:?}", plan.counts());
    println!("BEFORE {:?}", plan.readability_before);
    assert!(plan.readability.is_clean(), "{:#?}", plan.readability);
    assert!(plan.counts().removals > 0 && !plan.moves.is_empty());
    assert!(plan.findings.iter().any(|f| f.code == "TIDY_ANNOTATIONS_STAY"));

    let fingerprint = session.fingerprint().await.unwrap();
    let report = session.apply_plan(&plan, &fingerprint).await.unwrap();
    assert!(report.verified, "{:#?}", report.verification);
    assert_eq!(report.moved, plan.moves.len());
    assert_eq!(report.removed, plan.removals.len());

    // Tidying must not make ERC any unhappier than it was.
    let errors_after = erc_errors(&session, &schematic, &format!("{label}-after"), &installs).await;
    assert!(
        errors_after <= errors_before,
        "ERC errors went from {errors_before} to {errors_after}"
    );

    // The circuit is the circuit that was there.
    assert_eq!(topology(&session.netlist().await.unwrap()), before_nets);
    // Parts kept their identity, values, and footprints; the notes stayed.
    let design_after = session.design().await.unwrap();
    let parts = |d: &kicad_domain::SchematicDesign| {
        d.symbols
            .iter()
            .filter(|s| !s.reference.starts_with("#PWR"))
            .map(|s| {
                (
                    s.reference.clone(),
                    s.id.clone(),
                    s.value.clone(),
                    s.footprint.clone(),
                    s.library_id.clone(),
                )
            })
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(parts(&design_before), parts(&design_after));
    assert_eq!(design_after.annotations, design_before.annotations);
    render(&session, &schematic, &format!("{label}-after"), &installs).await;

    // Nothing is left to do, and the review agrees.
    let (_, again) = tidy_plan(&session).await;
    assert!(
        again.findings.iter().any(|f| f.code == "NOTHING_TO_CHANGE"),
        "{:#?}\n{:#?}\n{:#?}",
        again.moves,
        again.removals,
        again.operations
    );
    assert!(again.moves.is_empty() && again.removals.is_empty() && again.operations.is_empty());
    let after_check = kicad_domain::readability::check_design(
        &design_after,
        &session
            .library_symbols(&design_library_ids(&design_after))
            .await
            .unwrap(),
    );
    assert!(
        after_check.overlapping_symbols.is_empty() && after_check.wires_through_symbols.is_empty(),
        "{after_check:#?}"
    );
}

#[tokio::test]
async fn tidies_a_schematic_file() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let schematic = fixture_copy("file");
    let session = FileSession::open(
        &SessionConfig::default(),
        &installs,
        OpenTarget::Existing(schematic.clone()),
    )
    .await
    .unwrap();
    tidy_story(AnySession::from(session), schematic, "file", installs).await;
}

#[tokio::test]
async fn tidies_a_schematic_through_the_api() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let schematic = fixture_copy("api");
    let session = Session::open_headless(
        &SessionConfig::default(),
        &installs,
        OpenTarget::Existing(schematic.clone()),
    )
    .await
    .unwrap();
    tidy_story(AnySession::from(session), schematic, "api", installs).await;
}
