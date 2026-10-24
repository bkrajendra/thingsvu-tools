// SPDX-License-Identifier: GPL-3.0-or-later
//! Finding KiCad installs. Order of preference (plan §5.4): explicitly configured CLIs, then
//! `KICAD_HOME`, then `PATH`, then standard install locations. Every candidate is confirmed by
//! running `kicad-cli version --format about`.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::run::{run, CliError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallSource {
    Configured,
    KicadHome,
    Path,
    StandardLocation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KicadVersion {
    /// Full build string, e.g. `10.99.0-5896-gd033635628` or `10.0.7`.
    pub full: String,
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl KicadVersion {
    /// Parses the `Version:` line of `kicad-cli version --format about`, or a bare version.
    pub fn parse(text: &str) -> Option<Self> {
        let line = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("Version:"))
            .unwrap_or_else(|| text.lines().next().unwrap_or(""));
        let full = line.trim().split([',', ' ']).next()?.trim().to_owned();
        let mut numbers = full.split(['.', '-']).map(|p| p.parse::<u32>());
        let (major, minor, patch) = (numbers.next()?.ok()?, numbers.next()?.ok()?, numbers.next()?.ok()?);
        Some(Self {
            full,
            major,
            minor,
            patch,
        })
    }

    /// Development builds of the next major version use minor 99 (e.g. 10.99 before 11.0).
    pub fn is_development_build(&self) -> bool {
        self.minor >= 99
    }

    /// The KiCad release series this build belongs to: 10.99 counts as 11.
    pub fn series(&self) -> u32 {
        if self.is_development_build() {
            self.major + 1
        } else {
            self.major
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KicadInstall {
    pub cli: PathBuf,
    pub version: KicadVersion,
    pub source: InstallSource,
}

/// Inputs for candidate discovery, separated from the process environment for testing.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryEnv {
    pub kicad_home: Option<PathBuf>,
    pub path: Option<OsString>,
    /// Directories that contain one sub-directory per KiCad version (e.g. `C:\Program Files\KiCad`).
    pub install_roots: Vec<PathBuf>,
    /// Exact CLI locations used by platform installers.
    pub fixed_locations: Vec<PathBuf>,
}

pub const CLI_NAME: &str = if cfg!(windows) { "kicad-cli.exe" } else { "kicad-cli" };

impl DiscoveryEnv {
    pub fn from_process() -> Self {
        let mut install_roots = Vec::new();
        let mut fixed_locations = Vec::new();
        if cfg!(windows) {
            for var in ["ProgramFiles", "ProgramW6432"] {
                if let Some(dir) = std::env::var_os(var) {
                    install_roots.push(PathBuf::from(dir).join("KiCad"));
                }
            }
        } else if cfg!(target_os = "macos") {
            fixed_locations.push(PathBuf::from("/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli"));
        } else {
            fixed_locations.push(PathBuf::from("/usr/bin/kicad-cli"));
            fixed_locations.push(PathBuf::from("/usr/local/bin/kicad-cli"));
        }
        Self {
            kicad_home: std::env::var_os("KICAD_HOME").map(PathBuf::from),
            path: std::env::var_os("PATH"),
            install_roots,
            fixed_locations,
        }
    }

    /// Candidate CLI paths in preference order, without duplicates. Only existing files are kept.
    pub fn candidates(&self, configured: &[PathBuf]) -> Vec<(PathBuf, InstallSource)> {
        let mut out: Vec<(PathBuf, InstallSource)> = Vec::new();
        let mut seen = BTreeSet::new();
        let mut push = |path: PathBuf, source| {
            if path.is_file() {
                let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                if seen.insert(key) {
                    out.push((path, source));
                }
            }
        };
        for path in configured {
            push(path.clone(), InstallSource::Configured);
        }
        if let Some(home) = &self.kicad_home {
            push(home.join("bin").join(CLI_NAME), InstallSource::KicadHome);
        }
        if let Some(path) = &self.path {
            for dir in std::env::split_paths(path) {
                push(dir.join(CLI_NAME), InstallSource::Path);
            }
        }
        for root in &self.install_roots {
            let mut versions: Vec<PathBuf> = std::fs::read_dir(root)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            // Newest version directory first, comparing "10.0" > "9.0" numerically.
            versions.sort_by_key(|p| {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                std::cmp::Reverse(
                    name.split('.')
                        .map(|part| part.parse::<u32>().unwrap_or(0))
                        .collect::<Vec<_>>(),
                )
            });
            for dir in versions {
                push(dir.join("bin").join(CLI_NAME), InstallSource::StandardLocation);
            }
        }
        for path in &self.fixed_locations {
            push(path.clone(), InstallSource::StandardLocation);
        }
        out
    }
}

/// Runs `version --format about` on one CLI.
pub async fn identify(cli: &Path, source: InstallSource) -> Result<KicadInstall, CliError> {
    let args: Vec<OsString> = vec!["version".into(), "--format".into(), "about".into()];
    let output = run(cli, &args, Duration::from_secs(30)).await?;
    let version = KicadVersion::parse(&output.stdout).ok_or_else(|| {
        CliError::Unparseable(format!(
            "no version in output of {}: {}",
            cli.display(),
            output.stdout.trim()
        ))
    })?;
    Ok(KicadInstall {
        cli: cli.to_owned(),
        version,
        source,
    })
}

/// Finds and identifies every install. Candidates that fail to run are skipped.
pub async fn discover(configured: &[PathBuf]) -> Vec<KicadInstall> {
    let mut installs = Vec::new();
    for (cli, source) in DiscoveryEnv::from_process().candidates(configured) {
        if let Ok(install) = identify(&cli, source).await {
            installs.push(install);
        }
    }
    installs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_about_and_plain_output() {
        let about = "Application: kicad-cli x64 on x64\n\nVersion: 10.99.0-5896-gd033635628, release build\n";
        let v = KicadVersion::parse(about).unwrap();
        assert_eq!(
            (v.full.as_str(), v.major, v.minor, v.patch),
            ("10.99.0-5896-gd033635628", 10, 99, 0)
        );
        assert!(v.is_development_build());
        assert_eq!(v.series(), 11);

        let stable = KicadVersion::parse("Version: 10.0.7, release build").unwrap();
        assert_eq!(
            (stable.major, stable.minor, stable.patch, stable.series()),
            (10, 0, 7, 10)
        );
        assert_eq!(KicadVersion::parse("10.99.0\n").unwrap().minor, 99);
        assert!(KicadVersion::parse("no version here").is_none());
    }

    fn fake_install(root: &Path, version_dir: &str) -> PathBuf {
        let bin = root.join(version_dir).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let cli = bin.join(CLI_NAME);
        std::fs::write(&cli, b"").unwrap();
        cli
    }

    #[test]
    fn candidate_order_and_deduplication() {
        let tmp = std::env::temp_dir().join(format!("thingsvu-discovery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let programs = tmp.join("KiCad");
        let older = fake_install(&programs, "9.0");
        let stable = fake_install(&programs, "10.0");
        let nightly = fake_install(&tmp.join("other"), "10.99");

        let env = DiscoveryEnv {
            kicad_home: Some(tmp.join("other").join("10.99")),
            path: None,
            install_roots: vec![programs.clone()],
            fixed_locations: vec![],
        };
        assert_eq!(
            env.candidates(&[]),
            vec![
                (nightly.clone(), InstallSource::KicadHome),
                (stable.clone(), InstallSource::StandardLocation), // 10.0 before 9.0
                (older.clone(), InstallSource::StandardLocation),
            ]
        );
        // A configured CLI comes first; the same file found again later is not repeated.
        let configured = env.candidates(std::slice::from_ref(&stable));
        assert_eq!(configured[0], (stable.clone(), InstallSource::Configured));
        assert_eq!(configured.len(), 3);
        // PATH sits between KICAD_HOME and the standard locations.
        let with_path = DiscoveryEnv {
            path: Some(std::env::join_paths([older.parent().unwrap()]).unwrap()),
            ..env.clone()
        };
        assert_eq!(with_path.candidates(&[])[1], (older, InstallSource::Path));
        assert_eq!(env.candidates(&[tmp.join("missing.exe")]).len(), 3);
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
