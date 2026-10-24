// SPDX-License-Identifier: GPL-3.0-or-later
//! The `kicad-ai-mcp` server: MCP tools over KiCad sessions. stdout carries only MCP protocol
//! traffic; all diagnostics go to stderr.

pub mod envelope;
pub mod server;

pub use server::{KicadAiServer, Options};
