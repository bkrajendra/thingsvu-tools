// SPDX-License-Identifier: GPL-3.0-or-later
//! The file provider against a real KiCad 10 `kicad-cli` (and, for comparison and rendering, the
//! KiCad 11 nightly). Skipped unless `KICAD_LIVE=1`. Renders go to `THINGSVU_LAYOUT_OUT`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use kicad_domain::{plan_circuit, CircuitSpec};
use kicad_session::{FileSession, OpenTarget, Session, SessionConfig};

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

fn spec(file: &str) -> CircuitSpec {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/examples")
        .join(file);
    CircuitSpec::from_json(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn work_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("thingsvu-file-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Builds `spec_file` in a new schematic through the file provider and renders it with the
/// newest install (KiCad 10's CLI cannot export PNG).
async fn build_with_file_provider(spec_file: &str) -> PathBuf {
    let spec = spec(spec_file);
    let name = spec.name.to_string();
    let dir = work_dir(&name);
    let schematic = dir.join(format!("{name}.kicad_sch"));
    let installs = kicad_cli_runner::discover(&[]).await;
    let session = FileSession::open(&SessionConfig::default(), &installs, OpenTarget::New(schematic.clone()))
        .await
        .unwrap();
    assert_eq!(
        session.cli().unwrap().version.series(),
        10,
        "expected the KiCad 10 CLI to be used"
    );

    let ids = kicad_domain::library_ids_for(&spec);
    let library = session.library_symbols(&ids).await.unwrap();
    let plan = plan_circuit(&spec, &library, &session.existing_document().await.unwrap());
    assert!(
        !kicad_domain::validation::has_errors(&plan.findings),
        "{:#?}",
        plan.findings
    );
    assert!(plan.readability.is_clean(), "{:#?}", plan.readability);

    let fingerprint = session.fingerprint().await.unwrap();
    let report = session.apply_plan(&plan, &fingerprint).await.unwrap();
    assert!(report.verified, "{:#?}", report.verification);
    assert_eq!(report.placed.len(), spec.components.len());
    println!("FILE {name}: {:?} backup {:?}", plan.counts(), report.backup_path);

    // The file reads back the same through the session's own readers.
    let symbols = session.list_symbols().await.unwrap();
    assert!(symbols.iter().any(|s| s.reference == "J1"));
    let nets = session.netlist().await.unwrap();
    assert!(nets.iter().any(|n| n.name.trim_start_matches('/') == "GND"));

    // A second apply against the old fingerprint is refused and leaves the file alone.
    let before = std::fs::read(&schematic).unwrap();
    let stale = session.apply_plan(&plan, &fingerprint).await.unwrap_err();
    assert_eq!(stale.code, kicad_domain::ErrorCode::StalePlan);
    assert_eq!(std::fs::read(&schematic).unwrap(), before);

    let render = session.render_cli().expect("a KiCad 11 install renders PNG");
    let png_dir = dir.join("png");
    std::fs::create_dir_all(&png_dir).unwrap();
    let pngs = kicad_cli_runner::export::export_png(render, &schematic, &png_dir, 110, Duration::from_secs(120))
        .await
        .unwrap();
    let target = out_dir().join(format!("file-{name}.png"));
    std::fs::copy(&pngs[0], &target).unwrap();
    println!("RENDER {}", target.display());
    target
}

#[tokio::test]
async fn regulator_on_kicad_10() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let png = build_with_file_provider("regulator-3v3.json").await;
    assert!(Path::new(&png).exists());
}

#[tokio::test]
async fn ne555_on_kicad_10() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let png = build_with_file_provider("ne555-blinker.json").await;
    assert!(Path::new(&png).exists());
}

/// Symbol geometry read from the library files must equal what KiCad's API reports, because the
/// layout engine plans from it and the verification catches nothing about field text.
#[tokio::test]
async fn library_geometry_matches_kicads_own() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let dir = work_dir("geometry");
    let file = FileSession::open(
        &SessionConfig::default(),
        &installs,
        OpenTarget::New(dir.join("a.kicad_sch")),
    )
    .await
    .unwrap();
    let ipc = Session::open_headless(
        &SessionConfig::default(),
        &installs,
        OpenTarget::New(dir.join("b.kicad_sch")),
    )
    .await
    .unwrap();

    let mut ids = Vec::new();
    for f in ["regulator-3v3.json", "ne555-blinker.json"] {
        ids.extend(kicad_domain::library_ids_for(&spec(f)));
    }
    ids.sort();
    ids.dedup();
    let from_file = file.library_symbols(&ids).await.unwrap();
    let from_kicad = ipc.library_symbols(&ids).await.unwrap();
    // Signal nets also ask for a power symbol of their name, which usually does not exist.
    let found: Vec<_> = from_file.keys().collect();
    assert_eq!(
        found,
        from_kicad.keys().collect::<Vec<_>>(),
        "both find the same symbols"
    );
    assert!(found.len() >= 12, "{found:?}");
    for id in found {
        let (mut a, mut b) = (from_file[id].clone(), from_kicad[id].clone());
        for s in [&mut a, &mut b] {
            s.pins.sort_by(|x, y| (&x.number, x.unit).cmp(&(&y.number, y.unit)));
        }
        assert_eq!(a.reference_prefix, b.reference_prefix, "{id:?} prefix");
        assert_eq!(a.is_power, b.is_power, "{id:?} power");
        assert_eq!(a.unit_count, b.unit_count, "{id:?} units");
        assert_eq!(a.pins, b.pins, "{id:?} pins");
        assert_eq!(a.body, b.body, "{id:?} body");
    }
    ipc.close(true).await.unwrap();
}

/// Lines of `original` appear in `edited` in the same order: the edit only inserted text.
fn only_inserted(original: &str, edited: &str) -> bool {
    let mut rest = edited.lines();
    original.lines().all(|line| rest.any(|l| l == line))
}

/// A schematic KiCad itself wrote (a bundled demo, upgraded by the 10.0 CLI) gets two resistors
/// added: everything that was there stays, and its connectivity is unchanged.
#[tokio::test]
async fn edits_a_real_schematic_without_disturbing_it() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let cli10 = installs
        .iter()
        .find(|i| i.version.series() == 10)
        .expect("a KiCad 10 install");
    let demo = cli10
        .cli
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join("share/kicad/demos/simulation/rectifier");
    let dir = work_dir("real");
    for entry in std::fs::read_dir(&demo).unwrap().flatten() {
        if entry.path().is_file() {
            std::fs::copy(entry.path(), dir.join(entry.file_name())).unwrap();
        }
    }
    let schematic = dir.join("rectifier.kicad_sch");
    let status = std::process::Command::new(&cli10.cli)
        .args(["sch", "upgrade", "--force"])
        .arg(&schematic)
        .output()
        .unwrap();
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    let before = std::fs::read_to_string(&schematic).unwrap();
    let before_nets =
        kicad_cli_runner::netlist::export_netlist(cli10, &schematic, &dir.join("before.net"), Duration::from_secs(120))
            .await
            .unwrap();

    let session = FileSession::open(
        &SessionConfig::default(),
        &installs,
        OpenTarget::Existing(schematic.clone()),
    )
    .await
    .unwrap();
    let existing = session.list_symbols().await.unwrap();
    assert!(existing.len() > 3, "the demo has parts");

    let spec = spec("two-resistor-series-chain.json");
    let library = session
        .library_symbols(&kicad_domain::library_ids_for(&spec))
        .await
        .unwrap();
    let plan = plan_circuit(&spec, &library, &session.existing_document().await.unwrap());
    assert!(
        !kicad_domain::validation::has_errors(&plan.findings),
        "{:#?}",
        plan.findings
    );
    let fingerprint = session.fingerprint().await.unwrap();
    let report = session.apply_plan(&plan, &fingerprint).await.unwrap();
    assert!(report.verified, "{:#?}", report.verification);

    let after = std::fs::read_to_string(&schematic).unwrap();
    assert!(
        only_inserted(&before, &after),
        "the edit changed lines that were already there"
    );
    assert_eq!(session.list_symbols().await.unwrap().len(), existing.len() + 2);
    // New parts did not take references the demo already uses.
    let mut references: Vec<_> = session
        .list_symbols()
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.reference)
        .collect();
    references.sort();
    let unique = references.len();
    references.dedup();
    assert_eq!(references.len(), unique, "duplicate references");

    let after_nets =
        kicad_cli_runner::netlist::export_netlist(cli10, &schematic, &dir.join("after.net"), Duration::from_secs(120))
            .await
            .unwrap();
    let after_topology = after_nets.topology();
    for net in before_nets.topology() {
        assert!(after_topology.contains(&net), "net {net:?} changed");
    }
}

/// The parser and library reader cope with what KiCad actually ships.
#[tokio::test]
async fn parses_what_kicad_ships() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let cli10 = installs
        .iter()
        .find(|i| i.version.series() == 10)
        .expect("a KiCad 10 install");
    let root = cli10.cli.parent().and_then(Path::parent).unwrap().join("share/kicad");

    let mut symbols = 0;
    for name in [
        "Device",
        "power",
        "Connector_Generic",
        "Regulator_Linear",
        "Timer",
        "Diode",
        "LED",
        "Amplifier_Operational",
    ] {
        let text = std::fs::read_to_string(root.join("symbols").join(format!("{name}.kicad_sym"))).unwrap();
        let library = kicad_sexpr::SymbolLibrary::parse(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let tree = kicad_sexpr::parse(&text).unwrap();
        for entry in tree.children("symbol") {
            let entry_name = entry.arg_text(0).unwrap();
            let embedded = library
                .embeddable(name, entry_name)
                .unwrap_or_else(|e| panic!("{name}:{entry_name}: {e}"));
            // What goes into a schematic reads back as the same symbol.
            let again = kicad_sexpr::parse(&embedded.render(2)).unwrap();
            let id = kicad_domain::LibraryId::parse(&format!("{name}:{entry_name}")).unwrap();
            assert_eq!(
                kicad_sexpr::describe(&id, &embedded),
                kicad_sexpr::describe(&id, &again),
                "{id:?}"
            );
            symbols += 1;
        }
    }
    assert!(symbols > 300, "only {symbols} symbols");

    let mut files = 0;
    let mut stack = vec![root.join("demos")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("kicad_sch") && files < 60 {
                let text = std::fs::read_to_string(&path).unwrap();
                kicad_sexpr::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                files += 1;
            }
        }
    }
    assert!(files > 10, "only {files} demo schematics");
}

/// The same circuit built through KiCad 11's API and through the KiCad 10 file provider is the
/// same circuit: one CLI reads both files and reports identical connectivity.
#[tokio::test]
async fn both_backends_build_the_same_circuit() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let cli11 = installs
        .iter()
        .find(|i| i.version.series() >= 11)
        .expect("a KiCad 11 install");
    let dir = work_dir("contract");
    let mut topologies = Vec::new();
    for file_backend in [false, true] {
        let spec = spec("regulator-3v3.json");
        let path = dir.join(format!("{}.kicad_sch", if file_backend { "file" } else { "ipc" }));
        let config = SessionConfig::default();
        if file_backend {
            let session = FileSession::open(&config, &installs, OpenTarget::New(path.clone()))
                .await
                .unwrap();
            let library = session
                .library_symbols(&kicad_domain::library_ids_for(&spec))
                .await
                .unwrap();
            let plan = plan_circuit(&spec, &library, &session.existing_document().await.unwrap());
            let report = session
                .apply_plan(&plan, &session.fingerprint().await.unwrap())
                .await
                .unwrap();
            assert!(report.verified, "{:#?}", report.verification);
        } else {
            let session = Session::open_headless(&config, &installs, OpenTarget::New(path.clone()))
                .await
                .unwrap();
            let library = session
                .library_symbols(&kicad_domain::library_ids_for(&spec))
                .await
                .unwrap();
            let plan = plan_circuit(&spec, &library, &session.existing_document().await.unwrap());
            let report = session
                .apply_plan(&plan, &session.fingerprint().await.unwrap())
                .await
                .unwrap();
            assert!(report.verified, "{:#?}", report.verification);
            session.save().await.unwrap();
            session.close(false).await.unwrap();
        }
        let netlist = kicad_cli_runner::netlist::export_netlist(
            cli11,
            &path,
            &dir.join(format!("{file_backend}.net")),
            Duration::from_secs(120),
        )
        .await
        .unwrap();
        // Power symbols are not part of the circuit: compare real parts only.
        let topology: std::collections::BTreeSet<_> = netlist
            .topology()
            .into_iter()
            .map(|(name, pins)| {
                (
                    name,
                    pins.into_iter()
                        .filter(|p| !p.starts_with('#'))
                        .collect::<std::collections::BTreeSet<_>>(),
                )
            })
            .collect();
        topologies.push(topology);
    }
    assert_eq!(topologies[0], topologies[1]);
    assert_eq!(topologies[0].len(), 6, "{:?}", topologies[0]);
}
