// SPDX-License-Identifier: GPL-3.0-or-later
//! Phase 0 probe: drives a KiCad API server through the two-resistor series chain and verifies
//! connectivity from KiCad's own netlist.
//!
//! ```text
//! cargo run -p kicad-ipc --example probe -- <socket path> [basic|demo] [save-copy path]
//! ```
//!
//! `demo` modifies the loaded schematic; run it against a disposable project only.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use kicad_ipc::proto::kiapi::common::commands::{
    CreateItems, CreateItemsResponse, GetOpenDocuments, GetOpenDocumentsResponse, GetVersion, GetVersionResponse,
    LoadAllLibraries, PlaceFromLibraryResponse, SaveCopyOfDocument, SaveOptions, UpdateItems, UpdateItemsResponse,
};
use kicad_ipc::proto::kiapi::common::types::{
    DocumentSpecifier, DocumentType, ItemHeader, LibraryIdentifier, LibraryType, Text, Vector2,
};
use kicad_ipc::proto::kiapi::schematic::commands::{
    GetSchematicNetlist, PlaceSymbolFromLibrary, SchematicNetlistResponse,
};
use kicad_ipc::proto::kiapi::schematic::types::{
    LocalLabel, SchematicLine, SchematicLineType, SchematicPin, SchematicSymbolInstance,
};
use kicad_ipc::{Client, Error};
use prost::Name;
use prost_types::Any;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn mm(v: f64) -> i64 {
    (v * 1_000_000.0).round() as i64
}

fn point(x: f64, y: f64) -> Option<Vector2> {
    Some(Vector2 {
        x_nm: mm(x),
        y_nm: mm(y),
    })
}

fn kiid_str(id: &Option<kicad_ipc::proto::kiapi::common::types::Kiid>) -> String {
    id.as_ref().map(|k| k.value.clone()).unwrap_or_default()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let socket = PathBuf::from(
        args.next()
            .ok_or("usage: probe <socket path> [basic|demo] [save-copy path]")?,
    );
    let scenario = args.next().unwrap_or_else(|| "basic".into());
    let save_copy = args.next();

    let mut kicad = Client::connect(
        &socket,
        &format!("thingsvu-probe-{}", std::process::id()),
        Duration::from_secs(20),
    )
    .await?;
    let version: GetVersionResponse = kicad.call(&GetVersion {}).await?;
    let v = version.version.unwrap_or_default();
    println!(
        "KiCad {} (major {}, minor {}), instance token pinned: {}",
        v.full_version,
        v.major,
        v.minor,
        kicad.instance_token().is_some()
    );

    let docs: GetOpenDocumentsResponse = match kicad
        .call(&GetOpenDocuments {
            r#type: DocumentType::DoctypeSchematic as i32,
        })
        .await
    {
        Ok(d) => d,
        Err(e) if e.is_unhandled() => {
            println!("no schematic loaded ({e})");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    let doc: DocumentSpecifier = docs.documents.into_iter().next().ok_or("no open schematic")?;
    let project = doc.project.clone().unwrap_or_default();
    println!("schematic open in project '{}' at {}", project.name, project.path);
    if scenario != "demo" {
        return Ok(());
    }

    let header = || {
        Some(ItemHeader {
            document: Some(doc.clone()),
            ..Default::default()
        })
    };
    kicad
        .call_any(&LoadAllLibraries {
            r#type: vec![LibraryType::LtSymbol as i32],
        })
        .await?;

    let mut symbols = Vec::new();
    for (reference, x, y) in [("R1", 50.8, 50.8), ("R2", 50.8, 63.5)] {
        let request = PlaceSymbolFromLibrary {
            header: header(),
            lib_id: Some(LibraryIdentifier {
                library_nickname: "Device".into(),
                entry_name: "R".into(),
            }),
            position: point(x, y),
            reference: Some(reference.into()),
            ..Default::default()
        };
        let started = Instant::now();
        // Libraries load asynchronously in headless mode; placement fails until Device is ready.
        let placed: PlaceFromLibraryResponse = loop {
            match kicad.call(&request).await {
                Ok(r) => break r,
                Err(Error::Status { message, .. })
                    if message.contains("not found") && started.elapsed() < Duration::from_secs(90) =>
                {
                    tokio::time::sleep(Duration::from_millis(500)).await
                }
                Err(e) => return Err(e.into()),
            }
        };
        let symbol = placed
            .item
            .ok_or("placement returned no item")?
            .to_msg::<SchematicSymbolInstance>()?;
        println!(
            "placed {reference} after {:.1}s -> {}",
            started.elapsed().as_secs_f64(),
            kiid_str(&symbol.id)
        );
        symbols.push(symbol);
    }

    // Values: full-object update of each placed instance with an edited value field.
    let updates = symbols
        .iter()
        .map(|s| {
            let mut s = s.clone();
            if let Some(text) = s.value_field.as_mut().and_then(|f| f.text.as_mut()) {
                text.text = "10k".into();
            }
            Any::from_msg(&s)
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let updated: UpdateItemsResponse = kicad
        .call(&UpdateItems {
            header: header(),
            items: updates,
        })
        .await?;
    for result in &updated.updated_items {
        let s = result
            .item
            .as_ref()
            .ok_or("update returned no item")?
            .to_msg::<SchematicSymbolInstance>()?;
        let field = |f: &Option<kicad_ipc::proto::kiapi::schematic::types::SchematicField>| {
            f.as_ref()
                .and_then(|f| f.text.as_ref())
                .map(|t| t.text.clone())
                .unwrap_or_default()
        };
        println!(
            "updated {}: value = {}",
            field(&s.reference_field),
            field(&s.value_field)
        );
    }

    // Device:R pin 1 at local y = -3.81 mm, pin 2 at +3.81 mm; everything on the 1.27 mm grid.
    let wire = SchematicLine {
        start: point(50.8, 54.61),
        end: point(50.8, 59.69),
        r#type: SchematicLineType::SltWire as i32,
        ..Default::default()
    };
    let label = |name: &str, x: f64, y: f64| LocalLabel {
        position: point(x, y),
        text: Some(Text {
            position: point(x, y),
            text: name.into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let items = vec![
        Any::from_msg(&wire)?,
        Any::from_msg(&label("INPUT", 50.8, 46.99))?,
        Any::from_msg(&label("MID", 50.8, 57.15))?,
        Any::from_msg(&label("OUTPUT", 50.8, 67.31))?,
    ];
    let created: CreateItemsResponse = kicad
        .call(&CreateItems {
            header: header(),
            items,
            ..Default::default()
        })
        .await?;
    let failed = created
        .created_items
        .iter()
        .filter(|r| r.status.as_ref().map(|s| s.code) != Some(1))
        .count();
    println!("created {} items ({} failed)", created.created_items.len(), failed);

    // Read connectivity back and translate pin KIIDs into REF.PIN using each instance's pins.
    let mut pin_names = HashMap::new();
    for s in &symbols {
        let reference = s
            .reference_field
            .as_ref()
            .and_then(|f| f.text.as_ref())
            .map(|t| t.text.clone())
            .unwrap_or_default();
        for child in s.definition.iter().flat_map(|d| d.items.iter()) {
            if let Some(any) = &child.item {
                if any.type_url == SchematicPin::type_url() {
                    let pin = any.to_msg::<SchematicPin>()?;
                    pin_names.insert(kiid_str(&pin.id), format!("{reference}.{}", pin.number));
                }
            }
        }
    }
    let netlist: SchematicNetlistResponse = kicad
        .call(&GetSchematicNetlist {
            document: Some(doc.clone()),
            ..Default::default()
        })
        .await?;
    for net in &netlist.nets {
        let mut pins: Vec<_> = net
            .sheets
            .iter()
            .flat_map(|sheet| sheet.items.iter())
            .filter_map(|id| pin_names.get(&id.value).cloned())
            .collect();
        pins.sort();
        println!("net {:<10} pins {}", net.name, pins.join(", "));
    }

    if let Some(path) = save_copy {
        kicad
            .call_any(&SaveCopyOfDocument {
                document: Some(doc.clone()),
                path,
                options: Some(SaveOptions {
                    overwrite: false,
                    include_project: true,
                }),
            })
            .await?;
        println!("saved copy");
    }
    Ok(())
}
