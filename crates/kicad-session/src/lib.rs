// SPDX-License-Identifier: GPL-3.0-or-later
//! KiCad sessions for the MCP server (plan §3.1).
//!
//! A [`Session`] is one serialized connection to one KiCad instance and one target document,
//! with the capability set probed when it opened. Phase 1 implements the headless provider:
//! a `kicad-cli api-server` child owned by this process.

mod capability_map;
mod errors;
mod headless;
#[cfg(windows)]
mod job;
mod mutate;
mod schematic;
mod session;

pub use capability_map::capabilities_from_probe;
pub use errors::from_ipc_error;
pub use mutate::{ApplyReport, NetCheck, PlacedComponent};
pub use schematic::OpenDocument;
pub use session::{OpenTarget, Provider, Session, SessionConfig, SessionStatus};
