// SPDX-License-Identifier: GPL-3.0-or-later
//! Phase 9 probe: what the schematic API does with text, labels, shapes, and bounding boxes.
//! Skipped unless `KICAD_LIVE=1`; run with `--nocapture` to read the findings.

use std::path::PathBuf;

use kicad_ipc::proto::kiapi::common::commands::{
    CreateItems, CreateItemsResponse, DeleteItems, GetBoundingBox, GetBoundingBoxResponse, GetItems, GetItemsResponse,
};
use kicad_ipc::proto::kiapi::common::types::graphic_shape::Geometry;
use kicad_ipc::proto::kiapi::common::types::{
    Box2, Distance, GraphicAttributes, GraphicCircleAttributes, GraphicFillAttributes, GraphicFillType,
    GraphicRectangleAttributes, GraphicShape, ItemHeader, KiCadObjectType, Kiid, StrokeAttributes, Text, TextBox,
    Vector2,
};
use kicad_ipc::proto::kiapi::schematic::types::{
    LocalLabel, SchematicGraphicShape, SchematicLine, SchematicLineType, SchematicText, SchematicTextBox,
};
use kicad_session::{OpenTarget, Session, SessionConfig};
use prost::Name;
use prost_types::Any;

fn live() -> bool {
    std::env::var("KICAD_LIVE").as_deref() == Ok("1")
}

fn v(x: f64, y: f64) -> Option<Vector2> {
    Some(Vector2 {
        x_nm: (x * 1e6).round() as i64,
        y_nm: (y * 1e6).round() as i64,
    })
}

fn any<M: Name>(m: &M) -> Any {
    Any::from_msg(m).unwrap()
}

#[tokio::test]
async fn what_the_api_does_with_text_labels_and_shapes() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let dir = std::env::temp_dir().join(format!("thingsvu-probe9-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/v11/regulator-with-notes/regulator.kicad_sch");
    let schematic = dir.join("regulator.kicad_sch");
    std::fs::copy(source, &schematic).unwrap();
    let session = Session::open_headless(&SessionConfig::default(), &installs, OpenTarget::Existing(schematic))
        .await
        .unwrap();
    let header = || {
        Some(ItemHeader {
            document: Some(session.document_specifier().clone()),
            ..Default::default()
        })
    };

    // 1. Bounding boxes of existing parts, with and without their text.
    let symbols = session.list_symbols().await.unwrap();
    let ids: Vec<Kiid> = symbols
        .iter()
        .take(4)
        .map(|s| Kiid {
            value: s.id.as_str().to_owned(),
        })
        .collect();
    for mode in [1, 2] {
        let r: Result<GetBoundingBoxResponse, _> = session
            .raw_call(&GetBoundingBox {
                header: header(),
                items: ids.clone(),
                mode,
            })
            .await;
        match r {
            Ok(r) => {
                for (id, b) in r.items.iter().zip(&r.boxes) {
                    let name = symbols
                        .iter()
                        .find(|s| s.id.as_str() == id.value)
                        .map(|s| s.reference.clone());
                    let p = b.position.unwrap_or_default();
                    let sz = b.size.unwrap_or_default();
                    println!(
                        "BBOX mode={mode} {:?}: at ({:.2}, {:.2}) size {:.2} x {:.2} mm",
                        name,
                        p.x_nm as f64 / 1e6,
                        p.y_nm as f64 / 1e6,
                        sz.x_nm as f64 / 1e6,
                        sz.y_nm as f64 / 1e6
                    );
                }
            }
            Err(e) => println!("BBOX mode={mode} failed: {e}"),
        }
    }
    for s in symbols.iter().take(2) {
        println!("FIELD positions of {}: {:?}", s.reference, s.position_mm);
    }

    // 2. Create text, shapes, a graphic line, a text box, and a label.
    let stroke = Some(StrokeAttributes {
        width: Some(Distance { value_nm: 254_000 }),
        ..Default::default()
    });
    let attributes = Some(GraphicAttributes {
        stroke,
        fill: Some(GraphicFillAttributes {
            fill_type: GraphicFillType::GftUnfilled as i32,
            color: None,
        }),
    });
    let items = vec![
        any(&SchematicText {
            text: Some(Text {
                position: v(200.0, 30.0),
                text: "Power supply".into(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        any(&SchematicGraphicShape {
            shape: Some(GraphicShape {
                attributes,
                geometry: Some(Geometry::Rectangle(GraphicRectangleAttributes {
                    top_left: v(195.0, 25.0),
                    bottom_right: v(240.0, 60.0),
                    corner_radius: None,
                })),
                ..Default::default()
            }),
            ..Default::default()
        }),
        any(&SchematicGraphicShape {
            shape: Some(GraphicShape {
                attributes,
                geometry: Some(Geometry::Circle(GraphicCircleAttributes {
                    center: v(260.0, 40.0),
                    radius_point: v(265.0, 40.0),
                })),
                ..Default::default()
            }),
            ..Default::default()
        }),
        any(&SchematicLine {
            start: v(195.0, 70.0),
            end: v(240.0, 70.0),
            r#type: SchematicLineType::SltGraphic as i32,
            stroke,
            ..Default::default()
        }),
        any(&SchematicTextBox {
            textbox: Some(TextBox {
                top_left: v(195.0, 80.0),
                bottom_right: v(240.0, 95.0),
                text: "A boxed note".into(),
                border_enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        }),
        any(&LocalLabel {
            position: v(250.0, 100.0),
            text: Some(Text {
                position: v(250.0, 100.0),
                text: "PROBE".into(),
                ..Default::default()
            }),
            ..Default::default()
        }),
    ];
    let names = ["text", "rectangle", "circle", "graphic line", "text box", "local label"];
    let created: CreateItemsResponse = session
        .raw_call(&CreateItems {
            header: header(),
            items,
            ..Default::default()
        })
        .await
        .unwrap();
    let mut new_ids = Vec::new();
    for (name, c) in names.iter().zip(&created.created_items) {
        let status = c.status.clone().unwrap_or_default();
        println!("CREATE {name}: code={} {:?}", status.code, status.error_message);
        if let Some(item) = &c.item {
            println!("  type {}", item.type_url);
        }
        if status.code == 1 {
            if let Some(item) = &c.item {
                // The id is the first field of each message; read it generically.
                let raw = item.value.clone();
                println!("  {} bytes back", raw.len());
                new_ids.push(item.clone());
            }
        }
    }

    // 3. Read them back and ask for their boxes.
    let back: GetItemsResponse = session
        .raw_call(&GetItems {
            header: header(),
            types: [
                KiCadObjectType::KotSchText,
                KiCadObjectType::KotSchTextbox,
                KiCadObjectType::KotSchShape,
                KiCadObjectType::KotSchLine,
                KiCadObjectType::KotSchLabel,
            ]
            .map(|t| t as i32)
            .to_vec(),
        })
        .await
        .unwrap();
    let mut kinds: std::collections::BTreeMap<String, usize> = Default::default();
    let mut all_ids: Vec<Kiid> = Vec::new();
    for item in &back.items {
        *kinds.entry(item.type_url.clone()).or_default() += 1;
        let id = if item.type_url == SchematicText::type_url() {
            item.to_msg::<SchematicText>().ok().and_then(|m| m.id)
        } else if item.type_url == SchematicTextBox::type_url() {
            item.to_msg::<SchematicTextBox>().ok().and_then(|m| m.id)
        } else if item.type_url == SchematicGraphicShape::type_url() {
            item.to_msg::<SchematicGraphicShape>().ok().and_then(|m| m.id)
        } else if item.type_url == LocalLabel::type_url() {
            item.to_msg::<LocalLabel>().ok().and_then(|m| m.id)
        } else {
            None
        };
        all_ids.extend(id);
    }
    println!("READ BACK {kinds:#?}");
    let boxes: Result<GetBoundingBoxResponse, _> = session
        .raw_call(&GetBoundingBox {
            header: header(),
            items: all_ids.clone(),
            mode: 1,
        })
        .await;
    match boxes {
        Ok(r) => {
            for (id, b) in r.items.iter().zip(&r.boxes) {
                let Box2 { position, size } = *b;
                let (p, s) = (position.unwrap_or_default(), size.unwrap_or_default());
                println!(
                    "BOX {}: ({:.2}, {:.2}) {:.2} x {:.2}",
                    &id.value[..8],
                    p.x_nm as f64 / 1e6,
                    p.y_nm as f64 / 1e6,
                    s.x_nm as f64 / 1e6,
                    s.y_nm as f64 / 1e6
                );
            }
        }
        Err(e) => println!("BOX failed: {e}"),
    }

    // 4. What the file looks like.
    let copy_dir = dir.join("copy");
    std::fs::create_dir_all(&copy_dir).unwrap();
    let out = session.save_copy(&copy_dir).await.unwrap();
    println!("SAVE COPY {}", out.display());
    if let Ok(text) = std::fs::read_to_string(&out) {
        for tag in [
            "(text \"Power supply",
            "(rectangle",
            "(circle",
            "(polyline",
            "(text_box",
            "PROBE",
        ] {
            if let Some(i) = text.find(tag) {
                let end = text[i..]
                    .find("\n\t(")
                    .map(|e| e + i)
                    .unwrap_or(text.len())
                    .min(i + 700);
                println!("FILE {tag}: {}", &text[i..end]);
            } else {
                println!("FILE {tag}: not found");
            }
        }
    }

    // 5. Deleting what was created.
    let deleted: Result<kicad_ipc::proto::kiapi::common::commands::DeleteItemsResponse, _> = session
        .raw_call(&DeleteItems {
            header: header(),
            item_ids: all_ids.iter().filter(|_| false).cloned().collect(),
        })
        .await;
    println!("DELETE (nothing) {:?}", deleted.map(|d| d.deleted_items.len()));
    let _ = new_ids;

    // 6. The same fields read from the API and from the file.
    let api = session.design().await.unwrap();
    let file = kicad_session::FileSession::open(
        &SessionConfig::default(),
        &installs,
        kicad_session::OpenTarget::Existing(dir.join("copy").join("regulator.kicad_sch")),
    )
    .await;
    let file_design = match file {
        Ok(f) => Some(f.design().await.unwrap()),
        Err(e) => {
            println!("FILE SESSION failed: {e:?}");
            None
        }
    };
    for sym in api.symbols.iter().filter(|s| !s.reference.starts_with('#')).take(5) {
        println!(
            "API  {} at {:?} rot {} mirror {}{}",
            sym.reference,
            sym.position,
            sym.orientation.rotation_deg,
            sym.orientation.mirror_x,
            sym.orientation.mirror_y
        );
        for f in &sym.fields {
            println!(
                "   api  {} {:?} at ({:.2},{:.2}) angle {} size {} h {:?} v {:?} hidden {}",
                f.name,
                f.text,
                f.position.x.to_mm(),
                f.position.y.to_mm(),
                f.angle_deg,
                f.size.to_mm(),
                f.h,
                f.v,
                f.hidden
            );
        }
        if let Some(fd) = &file_design {
            if let Some(fs) = fd.symbols.iter().find(|x| x.reference == sym.reference) {
                for f in &fs.fields {
                    println!(
                        "   file {} {:?} at ({:.2},{:.2}) angle {} size {} h {:?} v {:?} hidden {}",
                        f.name,
                        f.text,
                        f.position.x.to_mm(),
                        f.position.y.to_mm(),
                        f.angle_deg,
                        f.size.to_mm(),
                        f.h,
                        f.v,
                        f.hidden
                    );
                }
            }
        }
    }
    for l in api.labels.iter().take(4) {
        println!("API label {:?} at {:?} dir {:?}", l.text, l.position, l.direction);
    }
    if let Some(fd) = &file_design {
        for l in fd.labels.iter().take(4) {
            println!("FILE label {:?} at {:?} dir {:?}", l.text, l.position, l.direction);
        }
        println!(
            "FILE texts {} shapes {} | API texts {} shapes {}",
            fd.texts.len(),
            fd.shapes.len(),
            api.texts.len(),
            api.shapes.len()
        );
    }
}
