// SPDX-License-Identifier: GPL-3.0-or-later
//! Layout review harness: plans, applies, verifies, saves, and renders each golden circuit.
//! Skipped unless `KICAD_LIVE=1`. Renders go to `THINGSVU_LAYOUT_OUT` (default: temp folder).

use std::path::{Path, PathBuf};
use std::time::Duration;

use kicad_domain::{plan_circuit, CircuitSpec};
use kicad_session::{OpenTarget, Session, SessionConfig};

fn out_dir() -> PathBuf {
    let dir = std::env::var_os("THINGSVU_LAYOUT_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("thingsvu-layout-review"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Builds `spec_file` in a new headless schematic and renders it to `<out>/<name>.png`.
async fn build_and_render(spec_file: &str) -> PathBuf {
    let spec_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/examples")
        .join(spec_file);
    let spec = CircuitSpec::from_json(&std::fs::read_to_string(&spec_path).unwrap()).unwrap();
    let name = spec.name.to_string();
    let work = std::env::temp_dir().join(format!("thingsvu-layout-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    let schematic = work.join(format!("{name}.kicad_sch"));

    let installs = kicad_cli_runner::discover(&[]).await;
    let session = Session::open_headless(&SessionConfig::default(), &installs, OpenTarget::New(schematic.clone()))
        .await
        .unwrap();
    let ids = kicad_domain::library_ids_for(&spec);
    let library = session.library_symbols(&ids).await.unwrap();
    let plan = plan_circuit(&spec, &library, &session.existing_document().await.unwrap());
    assert!(
        !kicad_domain::validation::has_errors(&plan.findings),
        "{:#?}",
        plan.findings
    );
    let fingerprint = session.fingerprint().await.unwrap();
    let report = session.apply_plan(&plan, &fingerprint).await.unwrap();
    assert!(report.verified, "{:#?}", report.verification);
    let counts = plan.counts();
    println!("LAYOUT {name}: {counts:?} fallback nets {:?}", plan.fallback_nets);
    for op in &plan.operations {
        if let kicad_domain::plan::Operation::PlaceSymbol {
            library_id,
            position,
            orientation,
            reference,
            ..
        }
        | kicad_domain::plan::Operation::PlacePowerSymbol {
            library_id,
            position,
            orientation,
            reference,
            ..
        } = op
        {
            if reference.starts_with("D") || reference == "#PWR11" {
                let s = &library[library_id];
                eprintln!(
                    "DBG {reference} {position:?} {orientation:?} body {:?} pins {:?}",
                    s.body,
                    s.pins.iter().map(|p| (p.local, p.outward)).collect::<Vec<_>>()
                );
            }
        }
    }
    assert!(plan.readability.is_clean(), "{name}: {:#?}", plan.readability);
    session.save().await.unwrap();
    session.close(false).await.unwrap();

    let cli = installs.iter().find(|i| i.version.series() >= 11).unwrap();
    let png_dir = work.join("png");
    std::fs::create_dir_all(&png_dir).unwrap();
    let pngs = kicad_cli_runner::export::export_png(cli, &schematic, &png_dir, 110, Duration::from_secs(120))
        .await
        .unwrap();
    let target = out_dir().join(format!("{name}.png"));
    std::fs::copy(&pngs[0], &target).unwrap();
    println!("RENDER {}", target.display());
    target
}

fn live() -> bool {
    std::env::var("KICAD_LIVE").as_deref() == Ok("1")
}

#[tokio::test]
async fn regulator_3v3() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let png = build_and_render("regulator-3v3.json").await;
    assert!(Path::new(&png).exists());
}

#[tokio::test]
async fn ne555_blinker() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let png = build_and_render("ne555-blinker.json").await;
    assert!(Path::new(&png).exists());
}
