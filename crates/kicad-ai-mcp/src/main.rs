// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;

use kicad_ai_mcp::{KicadAiServer, Options};
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: kicad-ai-mcp [--kicad-cli <path>]... [--read-only]

MCP server for KiCad over stdio. Logs go to stderr; set RUST_LOG to change the level.

  --kicad-cli <path>   kicad-cli to prefer over automatic discovery (repeatable)
  --read-only          refuse every tool that changes a schematic or writes into a project
  --help               show this help
  --version            show the version";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--kicad-cli" => options
                .configured_cli
                .push(PathBuf::from(args.next().ok_or("--kicad-cli needs a path")?)),
            "--read-only" => options.read_only = true,
            "--help" | "-h" => {
                eprintln!("{USAGE}");
                return Ok(());
            }
            "--version" | "-V" => {
                eprintln!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other => return Err(format!("unknown argument '{other}'\n\n{USAGE}").into()),
        }
    }

    // stdout is reserved for MCP messages.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        read_only = options.read_only,
        "kicad-ai-mcp starting on stdio"
    );
    let service = KicadAiServer::new(options).serve(rmcp::transport::stdio()).await?;
    let reason = service.waiting().await?;
    tracing::info!(?reason, "kicad-ai-mcp stopped");
    Ok(())
}
