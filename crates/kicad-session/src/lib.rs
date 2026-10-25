// SPDX-License-Identifier: GPL-3.0-or-later
//! KiCad sessions for the MCP server (plan §3.1).
//!
//! A [`Session`] is one serialized connection to one KiCad instance and one target document,
//! with the capability set probed when it opened. Phase 1 implements the headless provider:
//! a `kicad-cli api-server` child owned by this process.

mod any;
mod capability_map;
mod errors;
mod file;
mod headless;
#[cfg(windows)]
mod job;
mod mutate;
pub mod registry;
mod schematic;
mod session;

pub use any::AnySession;
pub use capability_map::capabilities_from_probe;
pub use errors::from_ipc_error;
pub use file::FileSession;
pub use mutate::{ApplyReport, NetCheck, PlacedComponent};
pub use schematic::OpenDocument;
pub use session::{OpenTarget, Provider, Session, SessionConfig, SessionStatus};
