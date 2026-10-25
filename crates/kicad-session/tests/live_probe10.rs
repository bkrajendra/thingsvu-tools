// SPDX-License-Identifier: GPL-3.0-or-later
//! Phase 10 probe: custom fields, part flags, and the BOM over the API and in the file.
//! Skipped unless `KICAD_LIVE=1`; run with `--nocapture` to read the findings.

use std::path::PathBuf;

use kicad_ipc::proto::kiapi::common::commands::{GetItems, GetItemsResponse, UpdateItems, UpdateItemsResponse};
use kicad_ipc::proto::kiapi::common::types::{ItemHeader, KiCadObjectType, Text};
use kicad_ipc::proto::kiapi::schematic::types::{SchematicField, SchematicSymbolAttributes, SchematicSymbolInstance};
use kicad_session::{OpenTarget, Session, SessionConfig};
use prost::Name;
use prost_types::{Any, FieldMask};

fn live() -> bool {
    std::env::var("KICAD_LIVE").as_deref() == Ok("1")
}

#[tokio::test]
async fn custom_fields_and_flags_over_the_api() {
    if !live() {
        eprintln!("skipped: set KICAD_LIVE=1");
        return;
    }
    let installs = kicad_cli_runner::discover(&[]).await;
    let dir = std::env::temp_dir().join(format!("thingsvu-probe10-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("regulator.kicad_sch");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/v11/regulator-with-notes/regulator.kicad_sch"),
        &schematic,
    )
    .unwrap();
    let session = Session::open_headless(&SessionConfig::default(), &installs, OpenTarget::Existing(schematic))
        .await
        .unwrap();
    let header = |paths: Vec<String>| {
        Some(ItemHeader {
            document: Some(session.document_specifier().clone()),
            field_mask: Some(FieldMask { paths }),
            ..Default::default()
        })
    };

    let read: GetItemsResponse = session
        .raw_call(&GetItems {
            header: header(vec![]),
            types: vec![KiCadObjectType::KotSchSymbol as i32],
        })
        .await
        .unwrap();
    let mut instances: Vec<SchematicSymbolInstance> = read
        .items
        .iter()
        .filter(|a| a.type_url == SchematicSymbolInstance::type_url())
        .map(|a| a.to_msg().unwrap())
        .collect();
    for i in instances.iter().filter(|i| {
        i.reference_field
            .as_ref()
            .is_some_and(|f| f.text.as_ref().is_some_and(|t| t.text == "C1" || t.text == "F1"))
    }) {
        println!(
            "BEFORE {:?}: attributes {:?} user_fields {:?} datasheet {:?} description {:?}",
            i.reference_field
                .as_ref()
                .and_then(|f| f.text.as_ref())
                .map(|t| t.text.clone()),
            i.attributes,
            i.user_fields
                .iter()
                .map(|f| (f.name.clone(), f.text.as_ref().map(|t| t.text.clone()), f.visible))
                .collect::<Vec<_>>(),
            i.datasheet_field
                .as_ref()
                .and_then(|f| f.text.as_ref())
                .map(|t| t.text.clone()),
            i.description_field
                .as_ref()
                .and_then(|f| f.text.as_ref())
                .map(|t| t.text.clone()),
        );
    }

    // A new custom field, hidden, and the do-not-populate flag, on C1.
    let c1 = instances
        .iter_mut()
        .find(|i| {
            i.reference_field
                .as_ref()
                .is_some_and(|f| f.text.as_ref().is_some_and(|t| t.text == "C1"))
        })
        .unwrap();
    let origin = c1.position;
    c1.definition = None;
    c1.user_fields.push(SchematicField {
        name: "MPN".into(),
        text: Some(Text {
            position: origin,
            text: "GRM188R71H104KA93D".into(),
            ..Default::default()
        }),
        visible: false,
        show_name: false,
        allow_auto_place: true,
        ..Default::default()
    });
    let update = |symbol: &SchematicSymbolInstance, paths: Vec<String>| UpdateItems {
        header: header(paths),
        items: vec![Any::from_msg(symbol).unwrap()],
    };
    let r: UpdateItemsResponse = session.raw_call(&update(c1, vec!["user_fields".into()])).await.unwrap();
    let status = r.updated_items[0].status.clone().unwrap_or_default();
    println!("UPDATE user_fields: code={} {:?}", status.code, status.error_message);
    if let Some(item) = &r.updated_items[0].item {
        let back: SchematicSymbolInstance = item.to_msg().unwrap();
        println!(
            "  back: {:?}",
            back.user_fields
                .iter()
                .map(|f| (f.name.clone(), f.text.as_ref().map(|t| t.text.clone()), f.visible))
                .collect::<Vec<_>>()
        );
    }

    c1.attributes = Some(SchematicSymbolAttributes {
        do_not_populate: true,
        exclude_from_bill_of_materials: false,
        ..c1.attributes.unwrap_or_default()
    });
    let r: UpdateItemsResponse = session.raw_call(&update(c1, vec!["attributes".into()])).await.unwrap();
    let status = r.updated_items[0].status.clone().unwrap_or_default();
    println!("UPDATE attributes: code={} {:?}", status.code, status.error_message);
    if let Some(item) = &r.updated_items[0].item {
        let back: SchematicSymbolInstance = item.to_msg().unwrap();
        println!("  back attributes: {:?}", back.attributes);
    }

    // The datasheet field (a standard field).
    let datasheet = c1.datasheet_field.clone();
    println!("DATASHEET field before: {datasheet:?}");
    if let Some(f) = c1.datasheet_field.as_mut() {
        if let Some(t) = f.text.as_mut() {
            t.text = "https://example.test/c1.pdf".into();
        }
    }
    let r: UpdateItemsResponse = session
        .raw_call(&update(c1, vec!["datasheet_field".into()]))
        .await
        .unwrap();
    let status = r.updated_items[0].status.clone().unwrap_or_default();
    println!("UPDATE datasheet: code={} {:?}", status.code, status.error_message);

    // And in the file.
    let copy_dir = dir.join("copy");
    std::fs::create_dir_all(&copy_dir).unwrap();
    let out = session.save_copy(&copy_dir).await.unwrap();
    let text = std::fs::read_to_string(out).unwrap();
    for needle in ["\"MPN\"", "(dnp yes)", "https://example.test"] {
        match text.find(needle) {
            Some(i) => {
                let start = text[..i].rfind("(property").unwrap_or(i.saturating_sub(40));
                println!(
                    "FILE {needle}: {}",
                    text[start.min(i)..(i + 260).min(text.len())]
                        .replace('\n', " ")
                        .replace('\t', "")
                );
            }
            None => println!("FILE {needle}: not found"),
        }
    }
}
