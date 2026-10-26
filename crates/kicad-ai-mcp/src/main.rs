// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;

use kicad_ai_mcp::{KicadAiServer, Options};
use rmcp::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

const USAGE: &str = r"usage: kicad-ai-mcp [--kicad-cli <path>]... [--read-only] [--freerouting <path>] [--java <path>]
                    [--log-dir <path>] [--no-log-file]

MCP server for KiCad over stdio. Logs go to stderr and to a daily rotating file (the last 7 days
are kept); set RUST_LOG to change the level.

  --kicad-cli <path>   kicad-cli to prefer over automatic discovery (repeatable)
  --read-only          refuse every tool that changes a schematic or writes into a project
  --freerouting <path> Freerouting for pcb_autoroute_plan: its native executable, or its jar
                       (or $THINGSVU_FREEROUTING, or freerouting in config.json, which
                       install.ps1 -WithFreerouting writes); autorouting is off without it
  --java <path>        Java for a Freerouting jar (or $THINGSVU_JAVA, $JAVA_HOME, the PATH)
  --log-dir <path>     folder for log files (default: %LOCALAPPDATA%\thingsvu\kicad-ai\logs,
                       or $THINGSVU_LOG_DIR)
  --no-log-file        log to stderr only
  --help               show this help
  --version            show the version";

/// Days of log files kept.
const LOG_DAYS: usize = 7;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut options = Options::default();
    let mut log_dir = Some(kicad_session::registry::log_dir());
    let (mut router_jar, mut java) = (None, None);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--kicad-cli" => options
                .configured_cli
                .push(PathBuf::from(args.next().ok_or("--kicad-cli needs a path")?)),
            "--read-only" => options.read_only = true,
            "--freerouting" | "--freerouting-jar" => {
                router_jar = Some(PathBuf::from(args.next().ok_or("--freerouting needs a path")?))
            }
            "--java" => java = Some(PathBuf::from(args.next().ok_or("--java needs a path")?)),
            "--log-dir" => log_dir = Some(PathBuf::from(args.next().ok_or("--log-dir needs a path")?)),
            "--no-log-file" => log_dir = None,
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

    let config = kicad_session::registry::config_file();
    options.freerouting = kicad_cli_runner::freerouting::Freerouting::configured(router_jar, java, Some(&config));

    // stdout is reserved for MCP messages. A log folder that cannot be used is not fatal: the
    // server still runs and logs to stderr.
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let (file_layer, _log_guard, log_note) = match &log_dir {
        Some(dir) => match tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("kicad-ai-mcp")
            .filename_suffix("log")
            .max_log_files(LOG_DAYS)
            .build(dir)
        {
            Ok(appender) => {
                let (writer, guard) = tracing_appender::non_blocking(appender);
                let layer = tracing_subscriber::fmt::layer()
                    .with_writer(writer)
                    .with_ansi(false)
                    .with_filter(filter());
                (Some(layer), Some(guard), None)
            }
            Err(e) => (None, None, Some(format!("cannot write logs to {}: {e}", dir.display()))),
        },
        None => (None, None, None),
    };
    use tracing_subscriber::Layer;
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .with_filter(filter()),
        )
        .with(file_layer)
        .init();
    if let Some(note) = log_note {
        tracing::warn!("{note}");
    }

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        read_only = options.read_only,
        freerouting = ?options.freerouting.as_ref().map(|f| f.program.display().to_string()),
        log_dir = ?log_dir,
        "kicad-ai-mcp starting on stdio"
    );
    let service = KicadAiServer::new(options).serve(rmcp::transport::stdio()).await?;
    let reason = service.waiting().await?;
    tracing::info!(?reason, "kicad-ai-mcp stopped");
    Ok(())
}
