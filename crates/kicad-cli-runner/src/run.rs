// SPDX-License-Identifier: GPL-3.0-or-later

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct CliOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("could not start {cli}: {source}")]
    Spawn { cli: PathBuf, source: std::io::Error },
    #[error("kicad-cli did not finish within {0:?}")]
    Timeout(Duration),
    #[error("kicad-cli could not load {file} (the file may be from a newer KiCad version): {stderr}")]
    LoadFailed { file: PathBuf, stderr: String },
    #[error("kicad-cli exited with {exit_code:?}: {stderr}")]
    Failed { exit_code: Option<i32>, stderr: String },
    #[error("kicad-cli reported success but wrote no readable report at {path}: {detail}")]
    BadReport { path: PathBuf, detail: String },
    #[error("unexpected kicad-cli output: {0}")]
    Unparseable(String),
}

/// Runs `cli` with `args`, capturing output. The child is killed if the deadline passes or the
/// returned future is dropped.
pub(crate) async fn run(cli: &Path, args: &[OsString], timeout: Duration) -> Result<CliOutput, CliError> {
    let mut command = tokio::process::Command::new(cli);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let child = command.spawn().map_err(|source| CliError::Spawn {
        cli: cli.to_owned(),
        source,
    })?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| CliError::Timeout(timeout))?
        .map_err(|source| CliError::Spawn {
            cli: cli.to_owned(),
            source,
        })?;
    Ok(CliOutput {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Turns a non-zero exit into a typed error. `kicad-cli` prints "Failed to load …" when it
/// cannot read the input, e.g. a KiCad 11 file given to the KiCad 10 CLI.
pub(crate) fn check(output: CliOutput, input: &Path) -> Result<CliOutput, CliError> {
    if output.exit_code == Some(0) {
        return Ok(output);
    }
    let stderr = if output.stderr.trim().is_empty() {
        output.stdout.trim().to_owned()
    } else {
        output.stderr.trim().to_owned()
    };
    if stderr.contains("Failed to load") {
        Err(CliError::LoadFailed {
            file: input.to_owned(),
            stderr,
        })
    } else {
        Err(CliError::Failed {
            exit_code: output.exit_code,
            stderr,
        })
    }
}
