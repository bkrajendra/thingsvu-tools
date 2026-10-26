// SPDX-License-Identifier: GPL-3.0-or-later
//! Freerouting, the optional external autorouter (Phase 12.7), used only when configured: the
//! native command-line executable Freerouting publishes since 2.5.0 (no Java needed), or the jar
//! with a Java runtime. The server never downloads it (the installer can, when asked). Each run is
//! a separate headless process on a Specctra DSN file that writes a session (`.ses`); it is killed
//! when the time limit passes. The GUI is off (the version check only runs with the GUI) and usage
//! data is turned off, so Freerouting makes no network calls.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::run::{run, CliError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Freerouting {
    /// The native executable, or the jar.
    pub program: PathBuf,
    /// The Java that runs the jar; none for the native executable.
    pub java: Option<PathBuf>,
}

/// What one routing run reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RouterRun {
    /// Connections Freerouting could not make, from its last summary line.
    pub unrouted: Option<u32>,
    /// Clearance violations it reports.
    pub violations: Option<u32>,
    pub seconds: f64,
    /// The last summary lines of its log, for the user.
    pub log_tail: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("could not start Freerouting ({program}): {source}")]
    Spawn { program: PathBuf, source: std::io::Error },
    #[error(
        "Freerouting did not finish within {0:?}; give a longer time limit or fewer passes. A build that          crashes can hang instead of exiting (Freerouting 2.5.0's native Windows build does on KiCad boards):          use its .jar, or run install.ps1 -FreeroutingOnly, which checks the router on a test board"
    )]
    Timeout(Duration),
    #[error("Freerouting exited with {exit_code:?} and wrote no session: {log}")]
    Failed { exit_code: Option<i32>, log: String },
}

impl Freerouting {
    /// The router at `program` (a native executable or a jar), if it exists. A jar runs with
    /// `java`, else `JAVA_HOME/bin/java`, else `java` on the PATH.
    pub fn at(program: PathBuf, java: Option<PathBuf>) -> Option<Freerouting> {
        if !program.is_file() {
            return None;
        }
        let is_jar = program.extension().is_some_and(|e| e.eq_ignore_ascii_case("jar"));
        let java = is_jar.then(|| {
            let exe = if cfg!(windows) { "java.exe" } else { "java" };
            java.or_else(|| {
                std::env::var_os("JAVA_HOME")
                    .map(|h| PathBuf::from(h).join("bin").join(exe))
                    .filter(|p| p.is_file())
            })
            .unwrap_or_else(|| PathBuf::from(exe))
        });
        Some(Freerouting { program, java })
    }

    /// The configured router, the first found of: the server's options, the environment
    /// (`THINGSVU_FREEROUTING` and `THINGSVU_JAVA`; `THINGSVU_FREEROUTING_JAR` is still read), and
    /// the settings file the installer writes (`{"freerouting": "...", "java": "..."}`).
    pub fn configured(program: Option<PathBuf>, java: Option<PathBuf>, config: Option<&Path>) -> Option<Freerouting> {
        let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let file: Option<serde_json::Value> = config
            .and_then(|c| std::fs::read_to_string(c).ok())
            // Windows PowerShell 5.1 writes UTF-8 with a byte-order mark.
            .and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok());
        let from_file = |k: &str| {
            file.as_ref()
                .and_then(|f| f.get(k))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        };
        let program = program
            .or_else(|| env("THINGSVU_FREEROUTING"))
            .or_else(|| env("THINGSVU_FREEROUTING_JAR"))
            .or_else(|| from_file("freerouting"))?;
        let java = java.or_else(|| env("THINGSVU_JAVA")).or_else(|| from_file("java"));
        Freerouting::at(program, java)
    }

    /// Routes `dsn` and writes the session to `ses`.
    pub async fn route(
        &self,
        dsn: &Path,
        ses: &Path,
        passes: u32,
        timeout: Duration,
    ) -> Result<RouterRun, RouterError> {
        let mut args: Vec<OsString> = Vec::new();
        if self.java.is_some() {
            args.push("-jar".into());
            args.push(self.program.clone().into_os_string());
        }
        args.extend([
            "-de".into(),
            dsn.as_os_str().to_owned(),
            "-do".into(),
            ses.as_os_str().to_owned(),
            format!("--router.autorouter.max_passes={passes}").into(),
            "--gui.enabled=false".into(),
            // Tracks keep the net class width into small pads, so DRC's minimum width holds.
            "--router.automatic_neckdown=false".into(),
            "--api_server.enabled=false".into(),
            "--mcp_server.enabled=false".into(),
            "--usage_and_diagnostic_data.disable_analytics=true".into(),
        ]);
        let launcher = self.java.clone().unwrap_or_else(|| self.program.clone());
        let started = Instant::now();
        let out = run(&launcher, &args, timeout).await.map_err(|e| match e {
            CliError::Timeout(t) => RouterError::Timeout(t),
            CliError::Spawn { cli, source } => RouterError::Spawn { program: cli, source },
            other => RouterError::Failed {
                exit_code: None,
                log: other.to_string(),
            },
        })?;
        let log = format!("{}\n{}", out.stdout, out.stderr);
        let (unrouted, violations) = summary(&log);
        let log_tail: Vec<String> = log
            .lines()
            .filter(|l| l.contains("stage completed") || l.contains("ERROR"))
            // Drop the timestamp and job id.
            .map(|l| l.split_once("] ").map(|(_, rest)| rest).unwrap_or(l).trim().to_owned())
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if !ses.is_file() {
            return Err(RouterError::Failed {
                exit_code: out.exit_code,
                log: log.lines().rev().take(8).collect::<Vec<_>>().join(" | "),
            });
        }
        Ok(RouterRun {
            unrouted,
            violations,
            seconds: (started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
            log_tail,
        })
    }
}

/// The last "(N unrouted and M violations)" in the log.
fn summary(log: &str) -> (Option<u32>, Option<u32>) {
    let mut found = (None, None);
    for line in log.lines() {
        let Some(i) = line.rfind(" unrouted and ") else {
            continue;
        };
        let before = &line[..i];
        let n = before.rsplit(['(', ' ']).next().and_then(|s| s.parse().ok());
        let after = &line[i + " unrouted and ".len()..];
        let m = after.split(' ').next().and_then(|s| s.parse().ok());
        if n.is_some() {
            found = (n, m);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_summary_line_counts() {
        let log = "INFO Auto-routing pass #1 ... with score 1000.00 (3 unrouted and 1 violations), using\n\
                   INFO Auto-routing stage completed: ... final score: 1000.00 (0 unrouted and 0 violations), using 0.89\n\
                   INFO Optimizer pass #1 ... (0 unrouted and 0 violations).";
        assert_eq!(summary(log), (Some(0), Some(0)));
        assert_eq!(summary("nothing"), (None, None));
    }

    #[test]
    fn a_missing_program_is_not_configured() {
        assert_eq!(Freerouting::at(PathBuf::from("Z:/no/such.jar"), None), None);
    }

    #[test]
    fn the_settings_file_names_the_router_and_a_jar_needs_java() {
        let dir = std::env::temp_dir().join(format!("thingsvu-fr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("freerouting-cli.exe");
        let jar = dir.join("freerouting.jar");
        std::fs::write(&exe, b"").unwrap();
        std::fs::write(&jar, b"").unwrap();
        let config = dir.join("config.json");
        // As the installer writes it on Windows PowerShell 5.1: with a byte-order mark.
        std::fs::write(
            &config,
            format!("\u{feff}{}", serde_json::json!({ "freerouting": exe })),
        )
        .unwrap();
        // The environment of the test run must not decide it.
        if std::env::var_os("THINGSVU_FREEROUTING").is_none() && std::env::var_os("THINGSVU_FREEROUTING_JAR").is_none()
        {
            let native = Freerouting::configured(None, None, Some(&config)).unwrap();
            assert_eq!((native.program, native.java), (exe.clone(), None));
        }
        let with_java = Freerouting::at(jar.clone(), Some(PathBuf::from("J:/java.exe"))).unwrap();
        assert_eq!(with_java.java, Some(PathBuf::from("J:/java.exe")));
        // An option wins over the file.
        let chosen = Freerouting::configured(Some(jar.clone()), None, Some(&config)).unwrap();
        assert_eq!(chosen.program, jar);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
