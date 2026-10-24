// SPDX-License-Identifier: GPL-3.0-or-later
//! Runs the real `kicad-cli` on the repository fixtures. Skipped unless `KICAD_LIVE=1`.

use std::path::PathBuf;
use std::time::Duration;

use kicad_cli_runner::{discover, erc, netlist, CliError};

fn live() -> bool {
    std::env::var("KICAD_LIVE").as_deref() == Ok("1")
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/v11/two-resistor-series-chain/kicad-demo.kicad_sch")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("thingsvu-cli-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[tokio::test]
async fn erc_and_netlist_on_the_kicad11_fixture() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1 to run against installed KiCad");
        return;
    }
    let installs = discover(&[]).await;
    let install = installs
        .iter()
        .find(|i| i.version.series() >= 11)
        .expect("a KiCad 11 (10.99) install");
    let timeout = Duration::from_secs(120);

    let report = erc::run_erc(install, &fixture(), &scratch("erc.json"), timeout)
        .await
        .unwrap();
    assert_eq!(report.count("error"), 0);
    assert_eq!(report.count("warning"), 2, "{report:#?}");

    let nets = netlist::export_netlist(install, &fixture(), &scratch("demo.net"), timeout)
        .await
        .unwrap();
    let mid = nets.net("/MID").expect("MID net");
    let mut pins: Vec<_> = mid.nodes.iter().map(|n| format!("{}.{}", n.reference, n.pin)).collect();
    pins.sort();
    assert_eq!(pins, ["R1.2", "R2.1"]);
}

#[tokio::test]
async fn kicad10_cli_reports_load_failure_for_a_kicad11_file() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1 to run against installed KiCad");
        return;
    }
    let installs = discover(&[]).await;
    let Some(stable) = installs.iter().find(|i| i.version.series() == 10) else {
        eprintln!("skipped: no KiCad 10 install found");
        return;
    };
    let err = netlist::export_netlist(stable, &fixture(), &scratch("v10.net"), Duration::from_secs(120))
        .await
        .unwrap_err();
    assert!(matches!(err, CliError::LoadFailed { .. }), "{err}");
}
