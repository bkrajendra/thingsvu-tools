// SPDX-License-Identifier: GPL-3.0-or-later
//! A KiCad that is busy, slow, replaced, or gone, as the session layer reports it. The mapping
//! decides what the agent is told to do next: retry, re-read first, or give up.

use std::time::Duration;

use kicad_domain::{ErrorCode, MutationState, ToolError};
use kicad_ipc::proto::kiapi::common::commands::{GetVersion, GetVersionResponse};
use kicad_ipc::proto::kiapi::common::types::KiCadVersion;
use kicad_ipc::proto::kiapi::common::ApiStatusCode;
use kicad_ipc::{Client, Error};
use kicad_ipc_mock::{Builder, MockKicad};
use kicad_session::from_ipc_error;

const FAST: Duration = Duration::from_millis(150);

fn version_reply() -> GetVersionResponse {
    GetVersionResponse {
        version: Some(KiCadVersion {
            major: 10,
            minor: 99,
            patch: 0,
            full_version: "10.99.0-test".into(),
        }),
    }
}

async fn client_to(mock: &MockKicad, timeout: Duration) -> Client {
    Client::connect(mock.socket_path(), "fault-test", timeout)
        .await
        .unwrap()
}

async fn get_version(client: &mut Client) -> Result<GetVersionResponse, Error> {
    client.call::<_, GetVersionResponse>(&GetVersion {}).await
}

/// Both readings of an error: for a request that only reads and for one that changes the design.
fn both(error: &Error) -> (ToolError, ToolError) {
    (from_ipc_error(error, false), from_ipc_error(error, true))
}

fn refusing(status: ApiStatusCode) -> Builder {
    MockKicad::builder().reply::<GetVersion, GetVersionResponse>(move |_| Err((status, "refused on purpose".into())))
}

#[tokio::test]
async fn a_busy_kicad_means_try_again() {
    let mock = refusing(ApiStatusCode::AsBusy).start().await.unwrap();
    let error = get_version(&mut client_to(&mock, FAST).await).await.unwrap_err();
    let (read, write) = both(&error);
    assert_eq!((read.code, read.retryable), (ErrorCode::KicadBusy, true));
    assert_eq!(read.mutation_state, MutationState::NotStarted);
    // KiCad answered, so a change was refused rather than half done.
    assert_eq!((write.code, write.retryable), (ErrorCode::KicadBusy, true));
    assert_eq!(write.mutation_state, MutationState::NotApplied);
}

#[tokio::test]
async fn a_kicad_that_lacks_the_command_is_a_missing_feature() {
    let mock = refusing(ApiStatusCode::AsUnhandled).start().await.unwrap();
    let error = get_version(&mut client_to(&mock, FAST).await).await.unwrap_err();
    assert_eq!(both(&error).0.code, ErrorCode::FeatureUnavailable);
}

#[tokio::test]
async fn a_rejected_request_is_the_callers_mistake() {
    let mock = refusing(ApiStatusCode::AsBadRequest).start().await.unwrap();
    let error = get_version(&mut client_to(&mock, FAST).await).await.unwrap_err();
    let (read, write) = both(&error);
    assert_eq!(read.code, ErrorCode::InvalidArgument);
    assert!(!read.retryable);
    assert_eq!(write.mutation_state, MutationState::NotApplied);
}

#[tokio::test]
async fn a_replaced_kicad_instance_is_noticed() {
    let mock = MockKicad::builder()
        .reply::<GetVersion, GetVersionResponse>(|_| Ok(version_reply()))
        .start()
        .await
        .unwrap();
    let mut client = client_to(&mock, FAST).await;
    // The session pinned the token of the KiCad it started with; this one answers with another.
    client.pin_instance_token("11111111-2222-4333-8444-555555555555");
    let error = get_version(&mut client).await.unwrap_err();
    let (read, write) = both(&error);
    assert_eq!(read.code, ErrorCode::KicadInstanceChanged, "{error}");
    assert_eq!(write.mutation_state, MutationState::NotApplied);
}

// The mock blocks its own thread to be slow, so it gets a runtime of its own; sharing one would
// freeze the client's timer too.
#[test]
fn a_slow_reply_to_a_read_is_busy_and_to_a_change_is_unknown() {
    let server = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let mock = server
        .block_on(
            MockKicad::builder()
                .handle::<GetVersion>(|_, _| {
                    std::thread::sleep(Duration::from_millis(600));
                    Ok(None)
                })
                .start(),
        )
        .unwrap();
    let client_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let error = client_rt
        .block_on(async { get_version(&mut client_to(&mock, FAST).await).await })
        .unwrap_err();
    assert!(matches!(error, Error::Timeout { .. }), "{error}");
    let (read, write) = both(&error);
    assert_eq!((read.code, read.retryable), (ErrorCode::KicadBusy, true));
    assert_eq!(read.mutation_state, MutationState::NotStarted);
    // A change may have been applied after the reply was lost: the agent must read back first.
    assert_eq!(write.code, ErrorCode::AmbiguousTimeout);
    assert_eq!(write.mutation_state, MutationState::Unknown);
    assert!(!write.retryable);
}

#[tokio::test]
async fn a_kicad_that_is_gone_is_a_disconnect() {
    let path = std::env::temp_dir().join(format!("thingsvu-nobody-{}.sock", std::process::id()));
    let error = Client::connect(&path, "fault-test", FAST)
        .await
        .err()
        .expect("nothing listens there");
    let (read, write) = both(&error);
    assert_eq!(read.code, ErrorCode::BridgeDisconnected, "{error}");
    assert_eq!(read.mutation_state, MutationState::NotStarted);
    // If the connection is lost during a change, whether it was applied is unknown.
    assert_eq!(write.mutation_state, MutationState::Unknown);
}
