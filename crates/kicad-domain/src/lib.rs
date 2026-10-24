// SPDX-License-Identifier: GPL-3.0-or-later
//! Stable domain model shared by the MCP server and every KiCad backend.
//!
//! Nothing here depends on how KiCad is reached (IPC, files, or CLI). Backends translate between
//! these types and KiCad's own representations.

pub mod capability;
pub mod circuit;
pub mod geometry;
pub mod ids;
pub mod layout;
pub mod plan;
pub mod readability;
pub mod result;
pub mod schematic;
pub mod validation;

pub use capability::{Backend, Capability, CapabilitySet, Support};
pub use circuit::{CircuitSpec, ComponentSpec, NetSpec, PinRef};
pub use geometry::{Dir, Nm, Orientation, Point, Rect, GRID};
pub use ids::{IdError, Kiid, LibraryId, LogicalId};
pub use layout::{is_ground_name, library_ids_for};
pub use plan::{
    plan_circuit, ExistingDocument, ExpectedNet, LibraryPin, LibrarySymbol, Operation, OperationCounts, OperationPlan,
};
pub use readability::Readability;
pub use result::{ErrorCode, MutationState, Origin, Provenance, ResultState, ToolError};
pub use schematic::{NetInfo, NetPin, PinInfo, PointMm, SymbolInfo, SymbolPins};
pub use validation::{validate_circuit, Finding, Severity};
