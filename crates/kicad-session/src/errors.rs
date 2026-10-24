// SPDX-License-Identifier: GPL-3.0-or-later

use kicad_domain::{ErrorCode, MutationState, ToolError};
use kicad_ipc::proto::kiapi::common::ApiStatusCode;
use kicad_ipc::Error;

/// Maps a KiCad IPC failure to a tool error. `mutating` says whether the request could have
/// changed the design, which decides what a timeout means.
pub fn from_ipc_error(error: &Error, mutating: bool) -> ToolError {
    let message = error.to_string();
    match error {
        Error::Timeout { .. } if mutating => {
            ToolError::new(ErrorCode::AmbiguousTimeout, message).with_mutation_state(MutationState::Unknown)
        }
        Error::Timeout { .. } => ToolError::new(ErrorCode::KicadBusy, message).retryable(),
        Error::Transport(_) => {
            ToolError::new(ErrorCode::BridgeDisconnected, message).with_mutation_state(if mutating {
                MutationState::Unknown
            } else {
                MutationState::NotStarted
            })
        }
        Error::Status { code, .. } => {
            let tool_error = match code {
                ApiStatusCode::AsBusy | ApiStatusCode::AsNotReady => {
                    ToolError::new(ErrorCode::KicadBusy, message).retryable()
                }
                ApiStatusCode::AsUnhandled | ApiStatusCode::AsUnimplemented => {
                    ToolError::new(ErrorCode::FeatureUnavailable, message)
                }
                ApiStatusCode::AsTokenMismatch => ToolError::new(ErrorCode::KicadInstanceChanged, message),
                ApiStatusCode::AsBadRequest => ToolError::new(ErrorCode::InvalidArgument, message),
                _ => ToolError::new(ErrorCode::KicadError, message),
            };
            // KiCad answered with a refusal, so nothing was applied.
            tool_error.with_mutation_state(if mutating {
                MutationState::NotApplied
            } else {
                MutationState::NotStarted
            })
        }
        _ => ToolError::new(ErrorCode::KicadError, message),
    }
}
