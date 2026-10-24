// SPDX-License-Identifier: GPL-3.0-or-later
//! Phase 0 probe: drives a KiCad API server through the two-resistor series chain and verifies
//! connectivity from KiCad's own netlist.
//!
//! ```text
//! cargo run -p kicad-ipc --example probe -- <socket path> [basic|demo|gui|revert] [output dir]
//! ```
//!
//! - `basic`: version and open documents.
//! - `demo`: builds the circuit; with an output dir, saves a copy there.
//! - `gui`: `demo` wrapped in one commit (if KiCad supports commits), plus a token-mismatch check
//!   and IPC netlist/SVG export jobs into the output dir.
//! - `revert`: reloads the open schematic from disk, discarding unsaved changes.
//! - `lifecycle` (headless, started without a file): create a schematic in the output dir, then
//!   check modified state, save, revert and close.
//!
//! `demo` and `gui` modify the loaded schematic; run them against a disposable project only.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use kicad_ipc::proto::kiapi::common::commands::{
    BeginCommit, BeginCommitResponse, CloseDocument, CommitAction, CreateDocument, CreateItems, CreateItemsResponse,
    DocumentModifiedState, EndCommit, GetDocumentModifiedState, GetDocumentModifiedStateResponse, GetOpenDocuments,
    GetOpenDocumentsResponse, GetVersion, GetVersionResponse, LoadAllLibraries, OpenDocumentResponse,
    PlaceFromLibraryResponse, RevertDocument, SaveCopyOfDocument, SaveDocument, SaveOptions, UpdateItems,
    UpdateItemsResponse,
};
use kicad_ipc::proto::kiapi::common::types::{
    DocumentSpecifier, DocumentType, ItemHeader, JobStatus, LibraryIdentifier, LibraryType, RunJobResponse,
    RunJobSettings, Text, Vector2,
};
use kicad_ipc::proto::kiapi::schematic::commands::{
    GetSchematicNetlist, PlaceSymbolFromLibrary, SchematicNetlistResponse,
};
use kicad_ipc::proto::kiapi::schematic::jobs::{
    RunSchematicJobExportNetlist, RunSchematicJobExportSvg, SchematicNetlistFormat,
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

async fn place_resistor(kicad: &mut Client, doc: &DocumentSpecifier, reference: &str, x: f64, y: f64) -> Result<()> {
    let request = PlaceSymbolFromLibrary {
        header: Some(ItemHeader {
            document: Some(doc.clone()),
            ..Default::default()
        }),
        lib_id: Some(LibraryIdentifier {
            library_nickname: "Device".into(),
            entry_name: "R".into(),
        }),
        position: point(x, y),
        reference: Some(reference.into()),
        ..Default::default()
    };
    let started = Instant::now();
    loop {
        match kicad.call::<_, PlaceFromLibraryResponse>(&request).await {
            Ok(_) => return Ok(()),
            Err(Error::Status { message, .. })
                if message.contains("not found") && started.elapsed() < Duration::from_secs(90) =>
            {
                tokio::time::sleep(Duration::from_millis(500)).await
            }
            Err(e) => return Err(e.into()),
        }
    }
}

async fn modified_state(kicad: &mut Client, doc: &DocumentSpecifier) -> Result<String> {
    let r: GetDocumentModifiedStateResponse = kicad
        .call(&GetDocumentModifiedState {
            document: Some(doc.clone()),
        })
        .await?;
    Ok(format!(
        "{:?}",
        DocumentModifiedState::try_from(r.state).unwrap_or(DocumentModifiedState::DmsUnknown)
    ))
}

async fn net_names(kicad: &mut Client, doc: &DocumentSpecifier) -> Result<Vec<String>> {
    let netlist: SchematicNetlistResponse = kicad
        .call(&GetSchematicNetlist {
            document: Some(doc.clone()),
            ..Default::default()
        })
        .await?;
    Ok(netlist.nets.into_iter().map(|n| n.name).collect())
}

async fn lifecycle(kicad: &mut Client, out_dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(out_dir)?;
    let path = out_dir.join("lifecycle.kicad_sch");
    let opened: OpenDocumentResponse = kicad
        .call(&CreateDocument {
            r#type: DocumentType::DoctypeSchematic as i32,
            path: path.display().to_string(),
        })
        .await?;
    let doc = opened.document.ok_or("CreateDocument returned no document")?;
    println!(
        "created {} -> state {}",
        path.display(),
        modified_state(kicad, &doc).await?
    );
    println!("file exists after create: {}", path.exists());

    kicad
        .call_any(&LoadAllLibraries {
            r#type: vec![LibraryType::LtSymbol as i32],
        })
        .await?;
    place_resistor(kicad, &doc, "R1", 50.8, 50.8).await?;
    println!("after placing R1: state {}", modified_state(kicad, &doc).await?);

    kicad
        .call_any(&SaveDocument {
            document: Some(doc.clone()),
        })
        .await?;
    let saved = std::fs::read_to_string(&path)?;
    println!(
        "after SaveDocument: state {}, file {} bytes, contains R1: {}",
        modified_state(kicad, &doc).await?,
        saved.len(),
        saved.contains("\"R1\"")
    );

    place_resistor(kicad, &doc, "R2", 50.8, 63.5).await?;
    println!(
        "after placing R2: state {}, nets {:?}",
        modified_state(kicad, &doc).await?,
        net_names(kicad, &doc).await?
    );
    kicad
        .call_any(&RevertDocument {
            document: Some(doc.clone()),
        })
        .await?;
    println!(
        "after RevertDocument: state {}, nets {:?}",
        modified_state(kicad, &doc).await?,
        net_names(kicad, &doc).await?
    );

    kicad
        .call_any(&CloseDocument {
            document: Some(doc.clone()),
        })
        .await?;
    match kicad
        .call::<_, GetOpenDocumentsResponse>(&GetOpenDocuments {
            r#type: DocumentType::DoctypeSchematic as i32,
        })
        .await
    {
        Ok(r) => println!("after CloseDocument: {} open schematic(s)", r.documents.len()),
        Err(e) => println!("after CloseDocument: {e}"),
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let socket = PathBuf::from(
        args.next()
            .ok_or("usage: probe <socket path> [basic|demo|gui|revert] [output dir]")?,
    );
    let scenario = args.next().unwrap_or_else(|| "basic".into());
    let out_dir = args.next().map(PathBuf::from);

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

    if scenario == "lifecycle" {
        return lifecycle(&mut kicad, &out_dir.ok_or("lifecycle needs an output dir")?).await;
    }

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
    let header = || {
        Some(ItemHeader {
            document: Some(doc.clone()),
            ..Default::default()
        })
    };

    if let Some(id) = scenario.strip_prefix("drop-commit=") {
        let result = kicad
            .call_any(&EndCommit {
                id: Some(kicad_ipc::proto::kiapi::common::types::Kiid { value: id.into() }),
                action: CommitAction::CmaDrop as i32,
                message: String::new(),
                header: header(),
            })
            .await;
        println!("drop commit {id}: {:?}", result.map(|_| "ok"));
        return Ok(());
    }
    if scenario == "revert" {
        kicad
            .call_any(&RevertDocument {
                document: Some(doc.clone()),
            })
            .await?;
        println!("reverted schematic to its saved state");
        return Ok(());
    }
    if scenario != "demo" && scenario != "gui" {
        return Ok(());
    }
    let gui = scenario == "gui";

    if gui {
        // A second client pinned to a wrong token must be refused before anything changes.
        let mut stranger = Client::connect(&socket, "thingsvu-probe-token-check", Duration::from_secs(20)).await?;
        stranger.pin_instance_token("00000000-0000-0000-0000-000000000000");
        match stranger.call::<_, GetVersionResponse>(&GetVersion {}).await {
            Err(Error::Status { code, message, .. }) => println!("wrong token -> {code:?}: {message}"),
            other => println!("wrong token -> unexpected: {:?}", other.map(|_| "accepted")),
        }
    }

    // One commit per apply when KiCad supports it (one undo step in the GUI).
    let commit_id = if gui {
        match kicad
            .call::<_, BeginCommitResponse>(&BeginCommit { header: header() })
            .await
        {
            Ok(r) => {
                println!("commit opened: {}", kiid_str(&r.id));
                r.id
            }
            Err(e) if e.is_unhandled() => {
                println!("commits not supported here ({e})");
                None
            }
            Err(e) => return Err(e.into()),
        }
    } else {
        None
    };
    // Headless needs an explicit library load; the GUI preloads and answers AS_UNIMPLEMENTED.
    match kicad
        .call_any(&LoadAllLibraries {
            r#type: vec![LibraryType::LtSymbol as i32],
        })
        .await
    {
        Ok(_) => {}
        Err(e) if e.is_unhandled() => println!("library load not needed here ({e})"),
        Err(e) => return Err(e.into()),
    }

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

    if commit_id.is_some() {
        kicad
            .call_any(&EndCommit {
                id: commit_id,
                action: CommitAction::CmaCommit as i32,
                message: "thingsvu probe: two-resistor demo".into(),
                header: header(),
            })
            .await?;
        println!("commit closed");
    }

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

    let Some(out_dir) = out_dir else { return Ok(()) };
    std::fs::create_dir_all(&out_dir)?;
    let in_out = |name: &str| out_dir.join(name).display().to_string();

    if gui {
        let settings = |output_path: String| {
            Some(RunJobSettings {
                document: Some(doc.clone()),
                output_path,
            })
        };
        let jobs: [(&str, Result<RunJobResponse>); 2] = [
            (
                "netlist job",
                kicad
                    .call(&RunSchematicJobExportNetlist {
                        job_settings: settings(in_out("ipc-netlist.net")),
                        format: SchematicNetlistFormat::SnfKicadSexpr as i32,
                        ..Default::default()
                    })
                    .await
                    .map_err(Into::into),
            ),
            (
                "SVG job",
                kicad
                    .call(&RunSchematicJobExportSvg {
                        job_settings: settings(out_dir.display().to_string()),
                        ..Default::default()
                    })
                    .await
                    .map_err(Into::into),
            ),
        ];
        for (name, result) in jobs {
            match result {
                Ok(r) => println!(
                    "{name}: {:?} -> {:?} {}",
                    JobStatus::try_from(r.status).unwrap_or(JobStatus::JsUnspecified),
                    r.output_path,
                    r.message
                ),
                Err(e) => println!("{name}: failed: {e}"),
            }
        }
    }

    kicad
        .call_any(&SaveCopyOfDocument {
            document: Some(doc.clone()),
            path: in_out(&format!("{}.kicad_sch", project.name)),
            options: Some(SaveOptions {
                overwrite: false,
                include_project: true,
            }),
        })
        .await?;
    println!("saved copy to {}", out_dir.display());
    Ok(())
}
