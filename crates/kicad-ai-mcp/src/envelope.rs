// SPDX-License-Identifier: GPL-3.0-or-later
//! The result envelope every tool returns (plan §7.4), as MCP structured content.

use std::sync::atomic::{AtomicU64, Ordering};

use kicad_domain::{Provenance, ResultState, ToolError};
use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::{json, Value};

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

pub fn correlation_id() -> String {
    format!("req-{:06}", NEXT_REQUEST.fetch_add(1, Ordering::Relaxed))
}

pub fn ok(
    correlation_id: &str,
    result: impl Serialize,
    provenance: Vec<Provenance>,
    warnings: Vec<String>,
) -> CallToolResult {
    let result = serde_json::to_value(result).unwrap_or_else(|e| json!({ "serialization_error": e.to_string() }));
    CallToolResult::structured(json!({
        "ok": true,
        "result_state": ResultState::Done,
        "result": result,
        "provenance": provenance,
        "warnings": warnings,
        "correlation_id": correlation_id,
    }))
}

pub fn error(correlation_id: &str, error: &ToolError) -> CallToolResult {
    tracing::warn!(correlation_id, code = ?error.code, "{}", error.message);
    CallToolResult::structured_error(json!({
        "ok": false,
        "result_state": error.result_state(),
        "error": {
            "code": error.code,
            "message": error.message,
            "retryable": error.retryable,
            "mutation_state": error.mutation_state,
            "correlation_id": correlation_id,
        },
    }))
}

/// For tests and logging: the envelope of a result.
pub fn structured(result: &CallToolResult) -> Option<&Value> {
    result.structured_content.as_ref()
}
