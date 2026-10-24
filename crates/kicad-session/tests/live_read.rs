// SPDX-License-Identifier: GPL-3.0-or-later
//! Phase 2 read tools against the installed nightly. Skipped unless `KICAD_LIVE=1`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kicad_domain::ErrorCode;
use kicad_ipc::proto::kiapi::common::commands::{
    CreateItems, CreateItemsResponse, PlaceFromLibraryResponse, UpdateItems, UpdateItemsResponse,
};
use kicad_ipc::proto::kiapi::common::types::{ItemHeader, LibraryIdentifier, Text, Vector2};
use kicad_ipc::proto::kiapi::schematic::commands::PlaceSymbolFromLibrary;
use kicad_ipc::proto::kiapi::schematic::types::{
    LocalLabel, SchematicSymbolInstance, SchematicSymbolOrientation, SchematicSymbolTransform,
};
use kicad_ipc::Error;
use kicad_session::{OpenTarget, Session, SessionConfig};
use prost_types::Any;

fn live() -> bool {
    std::env::var("KICAD_LIVE").as_deref() == Ok("1")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("thingsvu-read-live-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn fixture_copy(fixture: &str, dir: &Path) -> PathBuf {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/v11")
        .join(fixture);
    for file in ["kicad-demo.kicad_sch", "kicad-demo.kicad_pro"] {
        std::fs::copy(src.join(file), dir.join(file)).unwrap();
    }
    dir.join("kicad-demo.kicad_sch")
}

async fn open(target: OpenTarget) -> Session {
    let installs = kicad_cli_runner::discover(&[]).await;
    Session::open_headless(&SessionConfig::default(), &installs, target)
        .await
        .unwrap()
}

#[tokio::test]
async fn reads_the_two_resistor_fixture() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let session = open(OpenTarget::Existing(fixture_copy(
        "two-resistor-series-chain",
        &scratch("fixture"),
    )))
    .await;

    let symbols = session.list_symbols().await.unwrap();
    let summary: Vec<_> = symbols
        .iter()
        .map(|s| {
            (
                s.reference.as_str(),
                s.value.as_str(),
                s.library_id.as_str(),
                s.position_mm.x,
                s.position_mm.y,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("R1", "10k", "Device:R", 50.8, 50.8),
            ("R2", "10k", "Device:R", 50.8, 63.5)
        ]
    );
    assert!(symbols
        .iter()
        .all(|s| s.sheet_path == "/" && s.unit == 1 && s.orientation.rotation_deg == 0));

    let r1 = session.symbol_pins("R1").await.unwrap();
    let pins: Vec<_> = r1
        .pins
        .iter()
        .map(|p| {
            (
                p.number.as_str(),
                p.position_mm.x,
                p.position_mm.y,
                p.electrical_type.as_str(),
            )
        })
        .collect();
    assert_eq!(pins, [("1", 50.8, 46.99, "passive"), ("2", 50.8, 54.61, "passive")]);
    // Selecting by id gives the same symbol.
    assert_eq!(
        session
            .symbol_pins(r1.symbol.id.as_str())
            .await
            .unwrap()
            .symbol
            .reference,
        "R1"
    );
    assert_eq!(
        session.symbol_pins("R9").await.unwrap_err().code,
        ErrorCode::SymbolNotFound
    );

    let nets = session.netlist().await.unwrap();
    let topology: Vec<(String, Vec<String>)> = nets
        .iter()
        .map(|n| {
            (
                n.name.clone(),
                n.pins.iter().map(|p| format!("{}.{}", p.reference, p.pin)).collect(),
            )
        })
        .collect();
    assert_eq!(
        topology,
        [
            ("/INPUT".to_owned(), vec!["R1.1".to_owned()]),
            ("/MID".to_owned(), vec!["R1.2".to_owned(), "R2.1".to_owned()]),
            ("/OUTPUT".to_owned(), vec!["R2.2".to_owned()]),
        ]
    );
    // MID also carries one wire and one label.
    assert_eq!(nets[1].other_items, 2, "{:?}", nets[1]);

    let docs = session.open_documents().await.unwrap();
    assert!(docs.iter().any(|d| d.kind == "schematic"), "{docs:?}");
    session.close(true).await.unwrap();
}

fn mm(v: f64) -> i64 {
    (v * 1_000_000.0).round() as i64
}

/// Places Device:Q_NPN (asymmetric pins B, C, E) in every rotation, plain and mirrored, puts a label where
/// `Orientation::apply` says each pin is, and checks with KiCad's netlister that every label
/// touches exactly the pin it was computed for.
#[tokio::test]
async fn orientation_transform_matches_kicad_connectivity() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let dir = scratch("orientation");
    let session = open(OpenTarget::New(dir.join("orient.kicad_sch"))).await;
    let header = || {
        Some(ItemHeader {
            document: Some(session.document_specifier().clone()),
            ..Default::default()
        })
    };

    let rotations = [
        (0, SchematicSymbolOrientation::Sso0),
        (90, SchematicSymbolOrientation::Sso90),
        (180, SchematicSymbolOrientation::Sso180),
        (270, SchematicSymbolOrientation::Sso270),
    ];
    let mirrors = [(false, false), (true, false), (false, true)];
    let mut expected = Vec::new(); // (reference, rotation, mirror_x, mirror_y)

    for (row, &(mirror_x, mirror_y)) in mirrors.iter().enumerate() {
        for (col, &(deg, sso)) in rotations.iter().enumerate() {
            let reference = format!("Q{}", row * 4 + col + 1);
            let request = PlaceSymbolFromLibrary {
                header: header(),
                lib_id: Some(LibraryIdentifier {
                    library_nickname: "Device".into(),
                    entry_name: "Q_NPN".into(),
                }),
                position: Some(Vector2 {
                    x_nm: mm(25.4 + 25.4 * col as f64),
                    y_nm: mm(25.4 + 25.4 * row as f64),
                }),
                orientation: Some(sso as i32),
                reference: Some(reference.clone()),
                ..Default::default()
            };
            let started = Instant::now();
            let placed: PlaceFromLibraryResponse = loop {
                let result = session.raw_client().await.call(&request).await;
                match result {
                    Ok(r) => break r,
                    Err(Error::Status { message, .. })
                        if message.contains("not found") && started.elapsed() < Duration::from_secs(90) =>
                    {
                        tokio::time::sleep(Duration::from_millis(500)).await
                    }
                    Err(e) => panic!("placing {reference}: {e}"),
                }
            };
            if mirror_x || mirror_y {
                let mut symbol = placed.item.unwrap().to_msg::<SchematicSymbolInstance>().unwrap();
                let t = symbol.transform.get_or_insert(SchematicSymbolTransform::default());
                t.mirror_x = mirror_x;
                t.mirror_y = mirror_y;
                let updated: UpdateItemsResponse = session
                    .raw_client()
                    .await
                    .call(&UpdateItems {
                        header: header(),
                        items: vec![Any::from_msg(&symbol).unwrap()],
                    })
                    .await
                    .unwrap();
                assert_eq!(updated.updated_items.len(), 1);
            }
            expected.push((reference, deg, mirror_x, mirror_y));
        }
    }

    // KiCad may store an equivalent transform in a different form, so the requested and read-back
    // transforms are only reported; the connectivity check below is what must hold.
    let symbols = session.list_symbols().await.unwrap();
    for (reference, deg, mx, my) in &expected {
        let s = symbols.iter().find(|s| &s.reference == reference).unwrap();
        println!(
            "{reference}: requested rot {deg} mirror_x {mx} mirror_y {my} -> read back rot {} mirror_x {} mirror_y {}",
            s.orientation.rotation_deg, s.orientation.mirror_x, s.orientation.mirror_y
        );
    }

    // One label per pin, at the position our transform computes.
    let mut labels = Vec::new();
    for (reference, ..) in &expected {
        for pin in session.symbol_pins(reference).await.unwrap().pins {
            let (x, y) = (mm(pin.position_mm.x), mm(pin.position_mm.y));
            let name = format!("{reference}_{}", pin.number);
            labels.push(
                Any::from_msg(&LocalLabel {
                    position: Some(Vector2 { x_nm: x, y_nm: y }),
                    text: Some(Text {
                        position: Some(Vector2 { x_nm: x, y_nm: y }),
                        text: name,
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .unwrap(),
            );
        }
    }
    let created: CreateItemsResponse = session
        .raw_client()
        .await
        .call(&CreateItems {
            header: header(),
            items: labels,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(created.created_items.len(), expected.len() * 3);

    let nets = session.netlist().await.unwrap();
    if std::env::var("THINGSVU_DEBUG_NETS").is_ok() {
        for n in &nets {
            println!(
                "NET {} pins {:?} other {}",
                n.name,
                n.pins
                    .iter()
                    .map(|p| format!("{}.{}", p.reference, p.pin))
                    .collect::<Vec<_>>(),
                n.other_items
            );
        }
        for pin in session.symbol_pins("Q1").await.unwrap().pins {
            println!("Q1 PIN {} {:?} local {:?}", pin.number, pin.position_mm, pin.local_mm);
        }
    }
    let mut failures = Vec::new();
    for (reference, deg, mx, my) in &expected {
        for pin in ["B", "C", "E"] {
            let net_name = format!("/{reference}_{pin}");
            let got: Vec<String> = nets
                .iter()
                .find(|n| n.name == net_name)
                .map(|n| n.pins.iter().map(|p| format!("{}.{}", p.reference, p.pin)).collect())
                .unwrap_or_default();
            if got != [format!("{reference}.{pin}")] {
                failures.push(format!(
                    "rot {deg} mirror_x {mx} mirror_y {my}: label {net_name} touches {got:?}"
                ));
            }
        }
    }
    session.close(true).await.unwrap();
    assert!(
        failures.is_empty(),
        "transform disagrees with KiCad:\n{}",
        failures.join("\n")
    );
}

/// Phase 3 groundwork: which UpdateItems form changes a symbol's value without duplicating its
/// library symbol. Prints the outcome of each strategy; asserts nothing about which one wins.
#[tokio::test]
async fn value_update_strategies() {
    if !live() || std::env::var("THINGSVU_EXPERIMENT").is_err() {
        eprintln!("skipped: set KICAD_LIVE=1 and THINGSVU_EXPERIMENT=1");
        return;
    }
    use kicad_ipc::proto::kiapi::common::commands::{SaveCopyOfDocument, SaveOptions};
    for (name, mask, drop_definition) in [
        ("mask+no-def", true, true),
        ("no-mask+no-def", false, true),
        ("mask+def", true, false),
    ] {
        let dir = scratch(&format!("value-{name}"));
        let session = open(OpenTarget::New(dir.join("v.kicad_sch"))).await;
        let doc = session.document_specifier().clone();
        let request = PlaceSymbolFromLibrary {
            header: Some(ItemHeader {
                document: Some(doc.clone()),
                ..Default::default()
            }),
            lib_id: Some(LibraryIdentifier {
                library_nickname: "Device".into(),
                entry_name: "R".into(),
            }),
            position: Some(Vector2 {
                x_nm: mm(50.8),
                y_nm: mm(50.8),
            }),
            reference: Some("R1".into()),
            ..Default::default()
        };
        let started = Instant::now();
        let placed: PlaceFromLibraryResponse = loop {
            let result = session.raw_client().await.call(&request).await;
            match result {
                Ok(r) => break r,
                Err(Error::Status { message, .. })
                    if message.contains("not found") && started.elapsed() < Duration::from_secs(90) =>
                {
                    tokio::time::sleep(Duration::from_millis(500)).await
                }
                Err(e) => panic!("{e}"),
            }
        };
        let mut symbol = placed.item.unwrap().to_msg::<SchematicSymbolInstance>().unwrap();
        if drop_definition {
            symbol.definition = None;
        }
        if let Some(t) = symbol.value_field.as_mut().and_then(|f| f.text.as_mut()) {
            t.text = "10k".into();
        }
        let header = ItemHeader {
            document: Some(doc.clone()),
            field_mask: mask.then(|| prost_types::FieldMask {
                paths: vec!["value_field".into()],
            }),
            ..Default::default()
        };
        let result = session
            .raw_client()
            .await
            .call::<_, UpdateItemsResponse>(&UpdateItems {
                header: Some(header),
                items: vec![Any::from_msg(&symbol).unwrap()],
            })
            .await;
        let reply = match &result {
            Ok(r) => format!(
                "{:?}",
                r.updated_items
                    .iter()
                    .map(|i| i.status.as_ref().map(|s| (s.code, s.error_message.clone())))
                    .collect::<Vec<_>>()
            ),
            Err(e) => format!("error: {e}"),
        };
        let listed = session.list_symbols().await.unwrap();
        let out = dir.join("copy");
        std::fs::create_dir_all(&out).unwrap();
        session
            .raw_client()
            .await
            .call_any(&SaveCopyOfDocument {
                document: Some(doc.clone()),
                path: out.join("v.kicad_sch").display().to_string(),
                options: Some(SaveOptions {
                    overwrite: true,
                    include_project: true,
                }),
            })
            .await
            .unwrap();
        let file = std::fs::read_to_string(out.join("v.kicad_sch")).unwrap();
        let lib_symbols = file.lines().filter(|l| l.starts_with("\t\t(symbol \"")).count();
        let values: Vec<_> = file
            .lines()
            .filter(|l| l.trim_start().starts_with("(property \"Value\""))
            .map(str::trim)
            .collect();
        println!(
            "STRATEGY {name}: reply {reply}; read back value {:?} lib {:?}; file lib_symbols {lib_symbols}; values {values:?}",
            listed.first().map(|s| s.value.clone()),
            listed.first().map(|s| s.library_id.clone())
        );
        session.close(true).await.unwrap();
    }
}
