// SPDX-License-Identifier: GPL-3.0-or-later
//! Stable domain model shared by the MCP server and every KiCad backend.
//!
//! Nothing here depends on how KiCad is reached (IPC, files, or CLI). Backends translate between
//! these types and KiCad's own representations.

pub mod capability;
pub mod circuit;
pub mod connectivity;
pub mod geometry;
pub mod ids;
pub mod layout;
pub mod plan;
pub mod readability;
pub mod result;
pub mod schematic;
pub mod tidy;
pub mod validation;

pub use capability::{Backend, Capability, CapabilitySet, Support};
pub use circuit::{CircuitSpec, ComponentSpec, NetSpec, PinRef};
pub use connectivity::attach_power_pins;
pub use geometry::{Dir, Nm, Orientation, Point, Rect, GRID};
pub use ids::{IdError, Kiid, LibraryId, LogicalId};
pub use layout::{is_ground_name, library_ids_for};
pub use plan::{
    plan_circuit, ExistingDocument, ExpectedNet, LibraryPin, LibrarySymbol, Operation, OperationCounts, OperationPlan,
};
pub use readability::Readability;
pub use result::{ErrorCode, MutationState, Origin, Provenance, ResultState, ToolError};
pub use schematic::{
    DesignJunction, DesignLabel, DesignSymbol, DesignWire, NetInfo, NetPin, PinInfo, PointMm, SchematicDesign,
    SymbolInfo, SymbolPins,
};
pub use tidy::{design_library_ids, plan_tidy, tidy_spec};
pub use validation::{validate_circuit, Finding, Severity};
