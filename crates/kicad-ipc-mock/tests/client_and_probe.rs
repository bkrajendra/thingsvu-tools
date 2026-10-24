// SPDX-License-Identifier: GPL-3.0-or-later
//! The real client, transport, and capability probe against fake KiCad servers that reproduce
//! behaviour observed in Phase 0 (docs/kicad-api-compatibility.md).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use kicad_ipc::capabilities::{self, command, CommandSupport};
use kicad_ipc::proto::kiapi::common::commands::{
    BeginCommit, BeginCommitResponse, CommitAction, CreateItems, CreateItemsResponse, EndCommit, EndCommitResponse,
    GetDocumentModifiedState, GetDocumentModifiedStateResponse, GetOpenDocuments, GetOpenDocumentsResponse, GetVersion,
    GetVersionResponse,
};
use kicad_ipc::proto::kiapi::common::types::{
    document_specifier, DocumentSpecifier, DocumentType, KiCadVersion, Kiid, ProjectSpecifier,
};
use kicad_ipc::proto::kiapi::common::ApiStatusCode;
use kicad_ipc::proto::kiapi::schematic::commands::{
    GetSchematicHierarchy, GetSchematicNetlist, PlaceSymbolFromLibrary, SchematicHierarchyResponse,
    SchematicNetlistResponse,
};
use kicad_ipc::{Client, Error};
use kicad_ipc_mock::{Builder, MockKicad, MOCK_TOKEN};

const TIMEOUT: Duration = Duration::from_secs(5);

fn version(
    full: &str,
    major: u32,
    minor: u32,
) -> impl Fn(GetVersion) -> Result<GetVersionResponse, kicad_ipc_mock::Failure> {
    let full = full.to_owned();
    move |_| {
        Ok(GetVersionResponse {
            version: Some(KiCadVersion {
                major,
                minor,
                patch: 0,
                full_version: full.clone(),
            }),
        })
    }
}

/// KiCad 10.0.7 GUI: schematic named by filename only, commits answered, no schematic commands.
fn kicad10_gui(drops: Arc<Mutex<Vec<i32>>>) -> Builder {
    MockKicad::builder()
        .reply(version("10.0.7", 10, 0))
        .reply(|_: GetOpenDocuments| {
            Ok(GetOpenDocumentsResponse {
                documents: vec![DocumentSpecifier {
                    r#type: DocumentType::DoctypeSchematic as i32,
                    identifier: Some(document_specifier::Identifier::BoardFilename("demo.kicad_sch".into())),
                    project: None,
                }],
            })
        })
        .reply(|_: BeginCommit| {
            Ok(BeginCommitResponse {
                id: Some(Kiid {
                    value: "77444583-fbbe-413c-9019-9ad1a8eadb53".into(),
                }),
            })
        })
        .reply(move |req: EndCommit| {
            drops.lock().unwrap().push(req.action);
            Ok(EndCommitResponse {})
        })
}

/// KiCad 11 headless with a loaded schematic: schematic commands present, no commits.
fn kicad11_headless() -> Builder {
    let doc = DocumentSpecifier {
        r#type: DocumentType::DoctypeSchematic as i32,
        identifier: None,
        project: Some(ProjectSpecifier {
            name: "demo".into(),
            path: r"C:\work\demo\".into(),
        }),
    };
    let d = doc.clone();
    MockKicad::builder()
        .reply(version("10.99.0-5896-gd033635628", 10, 99))
        .reply(move |_: GetOpenDocuments| {
            Ok(GetOpenDocumentsResponse {
                documents: vec![d.clone()],
            })
        })
        .reply(|_: GetSchematicHierarchy| Ok(SchematicHierarchyResponse::default()))
        .reply(|_: GetSchematicNetlist| Ok(SchematicNetlistResponse::default()))
        .reply(|_: GetDocumentModifiedState| Ok(GetDocumentModifiedStateResponse { state: 1 }))
        .reply(|req: PlaceSymbolFromLibrary| -> Result<GetVersionResponse, _> {
            let lib = req.lib_id.unwrap_or_default();
            Err((
                ApiStatusCode::AsBadRequest,
                format!("symbol '{}:{}' not found", lib.library_nickname, lib.entry_name),
            ))
        })
        .reply(|req: CreateItems| {
            assert!(req.items.is_empty(), "probe must not create anything");
            Ok(CreateItemsResponse::default())
        })
}

#[tokio::test]
async fn pins_token_from_first_reply_and_sends_it_afterwards() {
    let mock = MockKicad::builder()
        .reply(version("10.99.0", 10, 99))
        .start()
        .await
        .unwrap();
    let mut client = Client::connect(mock.socket_path(), "test-client", TIMEOUT)
        .await
        .unwrap();
    assert_eq!(client.instance_token(), None);
    let v: GetVersionResponse = client.call(&GetVersion {}).await.unwrap();
    assert_eq!(v.version.unwrap().full_version, "10.99.0");
    assert_eq!(client.instance_token(), Some(MOCK_TOKEN));
    let _: GetVersionResponse = client.call(&GetVersion {}).await.unwrap();

    let received = mock.received();
    assert_eq!(received.len(), 2);
    assert_eq!(received[0].token, "");
    assert_eq!(received[1].token, MOCK_TOKEN);
    assert!(received.iter().all(|r| r.client_name == "test-client"));
}

#[tokio::test]
async fn maps_unhandled_and_token_mismatch() {
    let mock = MockKicad::builder()
        .reply(version("10.99.0", 10, 99))
        .start()
        .await
        .unwrap();
    let mut client = Client::connect(mock.socket_path(), "test-client", TIMEOUT)
        .await
        .unwrap();

    let err = client.call_any(&GetSchematicHierarchy::default()).await.unwrap_err();
    assert!(err.is_unhandled(), "{err}");

    client.pin_instance_token("11111111-2222-4333-8444-555555555555");
    match client.call::<_, GetVersionResponse>(&GetVersion {}).await {
        Err(Error::Status {
            code: ApiStatusCode::AsTokenMismatch,
            ..
        }) => {}
        other => panic!("expected token mismatch, got {:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn sequential_connections_are_served() {
    let mock = MockKicad::builder()
        .reply(version("10.99.0", 10, 99))
        .start()
        .await
        .unwrap();
    for _ in 0..3 {
        let mut client = Client::connect(mock.socket_path(), "test-client", TIMEOUT)
            .await
            .unwrap();
        let _: GetVersionResponse = client.call(&GetVersion {}).await.unwrap();
    }
    assert_eq!(mock.received().len(), 3);
}

#[tokio::test]
async fn probe_kicad10_gui_reports_missing_schematic_commands_and_drops_its_commit() {
    let drops = Arc::new(Mutex::new(Vec::new()));
    let mock = kicad10_gui(drops.clone()).start().await.unwrap();
    let mut client = Client::connect(mock.socket_path(), "probe", TIMEOUT).await.unwrap();
    let report = capabilities::probe(&mut client).await.unwrap();

    assert_eq!((report.major, report.minor), (10, 0));
    assert_eq!(report.schematics.len(), 1);
    assert!(report.supports(command::GET_OPEN_DOCUMENTS));
    assert!(report.supports(command::BEGIN_COMMIT));
    for missing in [
        command::GET_SCHEMATIC_HIERARCHY,
        command::GET_SCHEMATIC_NETLIST,
        command::PLACE_SYMBOL_FROM_LIBRARY,
        command::CREATE_ITEMS,
    ] {
        assert_eq!(report.commands[missing], CommandSupport::Unhandled, "{missing}");
    }
    assert_eq!(
        *drops.lock().unwrap(),
        vec![CommitAction::CmaDrop as i32],
        "probe commit must be dropped"
    );
}

#[tokio::test]
async fn probe_kicad11_headless_counts_a_not_found_rejection_as_supported() {
    let mock = kicad11_headless().start().await.unwrap();
    let mut client = Client::connect(mock.socket_path(), "probe", TIMEOUT).await.unwrap();
    let report = capabilities::probe(&mut client).await.unwrap();

    assert_eq!((report.major, report.minor), (10, 99));
    for present in [
        command::GET_SCHEMATIC_HIERARCHY,
        command::GET_SCHEMATIC_NETLIST,
        command::PLACE_SYMBOL_FROM_LIBRARY,
        command::CREATE_ITEMS,
        command::GET_DOCUMENT_MODIFIED_STATE,
    ] {
        assert!(report.supports(present), "{present}: {:?}", report.commands[present]);
    }
    assert_eq!(report.commands[command::BEGIN_COMMIT], CommandSupport::Unhandled);
}

#[tokio::test]
async fn probe_without_a_document_marks_document_probes_unknown() {
    let mock = MockKicad::builder()
        .reply(version("10.99.0", 10, 99))
        .start()
        .await
        .unwrap();
    let mut client = Client::connect(mock.socket_path(), "probe", TIMEOUT).await.unwrap();
    let report = capabilities::probe(&mut client).await.unwrap();
    assert_eq!(report.commands[command::GET_OPEN_DOCUMENTS], CommandSupport::Unhandled);
    assert!(matches!(
        report.commands[command::PLACE_SYMBOL_FROM_LIBRARY],
        CommandSupport::Unknown(_)
    ));
}
