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

/// Runs `cli` with `args`, capturing output. When the deadline passes, the child and every
/// process it started are ended (a router or exporter that hangs may have helpers of its own); if
/// the returned future is dropped, the child is killed.
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
    // Its own process group, so the whole group can be ended.
    #[cfg(unix)]
    command.process_group(0);
    let child = command.spawn().map_err(|source| CliError::Spawn {
        cli: cli.to_owned(),
        source,
    })?;
    let pid = child.id();
    let wait = child.wait_with_output();
    tokio::pin!(wait);
    let output = tokio::select! {
        output = &mut wait => output.map_err(|source| CliError::Spawn {
            cli: cli.to_owned(),
            source,
        })?,
        _ = tokio::time::sleep(timeout) => {
            // Ended while the child still exists: the tree is found through it.
            if let Some(pid) = pid {
                end_process_tree(pid).await;
            }
            return Err(CliError::Timeout(timeout));
        }
    };
    Ok(CliOutput {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Ends the process `pid` and everything it started.
async fn end_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut kill = tokio::process::Command::new("taskkill");
        kill.args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        let _ = kill.status().await;
    }
    #[cfg(unix)]
    {
        let mut kill = tokio::process::Command::new("kill");
        kill.args(["-KILL", "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = kill.status().await;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Is the process `pid` still there?
    fn alive(pid: u32) -> bool {
        #[cfg(windows)]
        {
            let out = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
        }
        #[cfg(unix)]
        {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .is_ok_and(|s| s.success())
        }
    }

    /// A program that starts a helper and hangs: when the time is up, the helper is ended too.
    #[tokio::test]
    async fn a_timeout_ends_the_helpers_of_the_program_as_well() {
        let dir = std::env::temp_dir().join(format!("thingsvu-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("helper.pid");
        let _ = std::fs::remove_file(&pid_file);
        #[cfg(windows)]
        let (program, args): (&str, Vec<OsString>) = (
            "powershell.exe",
            vec![
                "-NoProfile".into(),
                "-Command".into(),
                format!(
                    "$p = Start-Process -PassThru -WindowStyle Hidden ping -ArgumentList '-n','120','127.0.0.1'; \
                     Set-Content -Path '{}' -Value $p.Id; Start-Sleep 120",
                    pid_file.display()
                )
                .into(),
            ],
        );
        #[cfg(unix)]
        let (program, args): (&str, Vec<OsString>) = (
            "sh",
            vec![
                "-c".into(),
                format!("sleep 120 & echo $! > '{}'; wait", pid_file.display()).into(),
            ],
        );
        let started = std::time::Instant::now();
        let result = run(Path::new(program), &args, Duration::from_secs(6)).await;
        assert!(matches!(result, Err(CliError::Timeout(_))), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
        let helper: u32 = std::fs::read_to_string(&pid_file)
            .expect("the program started its helper before the time ran out")
            .trim_start_matches('\u{feff}')
            .trim()
            .parse()
            .unwrap();
        // The helper is gone (give the system a moment to clear it).
        let mut gone = false;
        for _ in 0..20 {
            if !alive(helper) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(gone, "the helper {helper} was left running");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_program_that_finishes_is_not_disturbed() {
        #[cfg(windows)]
        let (program, args): (&str, Vec<OsString>) = ("cmd.exe", vec!["/C".into(), "echo done".into()]);
        #[cfg(unix)]
        let (program, args): (&str, Vec<OsString>) = ("sh", vec!["-c".into(), "echo done".into()]);
        let out = run(Path::new(program), &args, Duration::from_secs(30)).await.unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout.contains("done"));
    }
}
