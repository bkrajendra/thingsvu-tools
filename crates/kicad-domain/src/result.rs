// SPDX-License-Identifier: GPL-3.0-or-later
//! The result envelope every tool returns (plan §7.4).

use serde::{Deserialize, Serialize};

/// How far an operation got. A check that did not run is `NotRun`, never a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultState {
    Done,
    Partial,
    Rejected,
    Unknown,
    NotRun,
}

/// Where a piece of data came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    KicadIpc,
    ProjectFile,
    KicadCli,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub origin: Origin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kicad_version: Option<String>,
    /// Session provider (`headless`, `plugin`, `direct`, `file`) when relevant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<String>,
    /// Document revision or content hash the data was read from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

/// Stable error codes (plan §5.5). Agents may branch on these; never rename one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    NoSession,
    BridgeDisconnected,
    KicadBusy,
    KicadInstanceChanged,
    FeatureUnavailable,
    DocumentNotOpen,
    DocumentOpenElsewhere,
    FileChangedOnDisk,
    StalePlan,
    SymbolNotFound,
    InvalidGridCoordinate,
    InvalidArgument,
    AmbiguousTimeout,
    ErcUnavailable,
    ReadOnlyMode,
    KicadNotFound,
    KicadError,
}

/// Whether a failed operation changed the design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationState {
    /// Read-only operation, or failed before anything was sent.
    NotStarted,
    /// KiCad refused it; nothing changed.
    NotApplied,
    /// Some or all changes were applied.
    Applied,
    /// A change may have been applied; read back before retrying.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct ToolError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    pub mutation_state: MutationState,
}

impl ToolError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
            mutation_state: MutationState::NotStarted,
        }
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    pub fn with_mutation_state(mut self, state: MutationState) -> Self {
        self.mutation_state = state;
        self
    }

    /// The `result_state` that matches this error.
    pub fn result_state(&self) -> ResultState {
        match self.mutation_state {
            MutationState::NotStarted | MutationState::NotApplied => ResultState::Rejected,
            MutationState::Applied => ResultState::Partial,
            MutationState::Unknown => ResultState::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_names() {
        assert_eq!(
            serde_json::to_value(ErrorCode::FeatureUnavailable).unwrap(),
            "FEATURE_UNAVAILABLE"
        );
        assert_eq!(serde_json::to_value(ResultState::NotRun).unwrap(), "not_run");
        assert_eq!(serde_json::to_value(Origin::ProjectFile).unwrap(), "project_file");
    }

    #[test]
    fn error_result_state() {
        let e = ToolError::new(ErrorCode::AmbiguousTimeout, "no reply").with_mutation_state(MutationState::Unknown);
        assert_eq!(e.result_state(), ResultState::Unknown);
        assert_eq!(
            ToolError::new(ErrorCode::NoSession, "x").result_state(),
            ResultState::Rejected
        );
    }
}
