// SPDX-License-Identifier: GPL-3.0-or-later
//! Freerouting, the optional external autorouter (Phase 12.7): found only when configured
//! (`--freerouting-jar`, or `THINGSVU_FREEROUTING_JAR`), never downloaded. Each run is a separate
//! headless process on a Specctra DSN file that writes a session (`.ses`); it is killed when the
//! time limit passes. The GUI is off (the version check only runs with the GUI) and usage data is
//! turned off, so Freerouting makes no network calls.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::run::{run, CliError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Freerouting {
    pub java: PathBuf,
    pub jar: PathBuf,
}

/// What one routing run reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RouterRun {
    /// Connections Freerouting could not make, from its last summary line.
    pub unrouted: Option<u32>,
    /// Clearance violations it reports.
    pub violations: Option<u32>,
    pub seconds: f64,
    /// The last lines of its log, for the user.
    pub log_tail: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("could not start Java ({java}): {source}")]
    Spawn { java: PathBuf, source: std::io::Error },
    #[error("Freerouting did not finish within {0:?}; give a longer time limit or fewer passes")]
    Timeout(Duration),
    #[error("Freerouting exited with {exit_code:?} and wrote no session: {log}")]
    Failed { exit_code: Option<i32>, log: String },
}

impl Freerouting {
    /// The configured router, if any: `jar` (else `THINGSVU_FREEROUTING_JAR`) must exist; Java is
    /// `java` (else `THINGSVU_JAVA`, else `JAVA_HOME/bin/java`, else `java` on the PATH).
    pub fn configured(jar: Option<PathBuf>, java: Option<PathBuf>) -> Option<Freerouting> {
        let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let jar = jar.or_else(|| env("THINGSVU_FREEROUTING_JAR"))?;
        if !jar.is_file() {
            return None;
        }
        let exe = if cfg!(windows) { "java.exe" } else { "java" };
        let java = java
            .or_else(|| env("THINGSVU_JAVA"))
            .or_else(|| {
                env("JAVA_HOME")
                    .map(|h| h.join("bin").join(exe))
                    .filter(|p| p.is_file())
            })
            .unwrap_or_else(|| PathBuf::from(exe));
        Some(Freerouting { java, jar })
    }

    /// Routes `dsn` and writes the session to `ses`.
    pub async fn route(
        &self,
        dsn: &Path,
        ses: &Path,
        passes: u32,
        timeout: Duration,
    ) -> Result<RouterRun, RouterError> {
        let args: Vec<OsString> = vec![
            "-jar".into(),
            self.jar.clone().into_os_string(),
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
        ];
        let started = Instant::now();
        let out = run(&self.java, &args, timeout).await.map_err(|e| match e {
            CliError::Timeout(t) => RouterError::Timeout(t),
            CliError::Spawn { cli, source } => RouterError::Spawn { java: cli, source },
            other => RouterError::Failed {
                exit_code: None,
                log: other.to_string(),
            },
        })?;
        let log = format!("{}\n{}", out.stdout, out.stderr);
        let (unrouted, violations) = summary(&log);
        let log_tail: Vec<String> = log
            .lines()
            .filter(|l| !l.trim().is_empty())
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
    fn a_missing_jar_is_not_configured() {
        assert_eq!(
            Freerouting::configured(Some(PathBuf::from("Z:/no/such.jar")), None),
            None
        );
    }
}
