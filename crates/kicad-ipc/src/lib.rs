// SPDX-License-Identifier: GPL-3.0-or-later
//! Client for the KiCad IPC API.
//!
//! - [`proto`]: types generated from KiCad's API protos (vendored in `proto/`).
//! - [`transport`]: a native implementation of the subset of NNG that KiCad's API server uses
//!   (REQ0 over the NNG ipc transport: a Windows named pipe or a Unix domain socket).
//! - [`Client`]: request/reply with the KiCad envelope, instance-token pinning and status mapping.

pub mod client;
pub mod transport;

pub use client::{Client, Error};

/// Types generated from KiCad's API protos. Do not edit; regenerate from `proto/`.
#[allow(clippy::all, missing_docs)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/kiapi.rs"));
}
