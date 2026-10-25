// SPDX-License-Identifier: GPL-3.0-or-later
//! The instance token pins a session to one KiCad and authorizes requests to it, so it must never
//! reach a log (plan §5.6). This reads every `tracing` call in the workspace and fails on one that
//! mentions a token.

use std::path::{Path, PathBuf};

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n != "target") {
                sources(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_log_call_mentions_a_token() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    sources(&crates, &mut files);
    assert!(files.len() > 30, "found only {} source files", files.len());
    let mut calls = 0;
    let mut offenders = Vec::new();
    for file in files {
        // Test code may talk about tokens; shipped code may not log them.
        if file.components().any(|c| c.as_os_str() == "tests") {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        let mut rest = text.as_str();
        while let Some(at) = rest.find("tracing::") {
            rest = &rest[at..];
            let end = rest.find(");").map_or(rest.len(), |e| e + 2);
            let call = &rest[..end];
            if call.contains("!(") {
                calls += 1;
                if call.to_lowercase().contains("token") {
                    offenders.push(format!("{}: {}", file.display(), call.lines().next().unwrap_or("")));
                }
            }
            rest = &rest["tracing::".len()..];
        }
    }
    assert!(
        calls > 5,
        "found only {calls} log calls; the scan is not looking in the right place"
    );
    assert!(offenders.is_empty(), "log calls that mention a token: {offenders:#?}");
}
