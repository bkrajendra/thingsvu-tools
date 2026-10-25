// SPDX-License-Identifier: GPL-3.0-or-later
//! The file provider when things go wrong: `kicad-cli` crashes, hangs, or answers nonsense; the
//! file changes under the session; an editor opens it. Whatever happens, the schematic is either
//! exactly as it was or exactly as planned — never half-written — and no verification copy stays
//! behind.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kicad_cli_runner::{InstallSource, KicadInstall, KicadVersion};
use kicad_domain::{ErrorCode, ExpectedNet, MutationState, OperationPlan, Readability};
use kicad_session::{FileSession, OpenTarget, SessionConfig};
use kicad_sexpr::{empty_schematic, MIN_VERSION};

struct Fixture {
    dir: PathBuf,
    schematic: PathBuf,
}

/// A fresh empty schematic whose fake `kicad-cli` behaves as `mode` says.
fn fixture(name: &str, mode: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("thingsvu-fault-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let schematic = dir.join("a.kicad_sch");
    std::fs::write(&schematic, empty_schematic(MIN_VERSION, "10.0")).unwrap();
    std::fs::write(dir.join(".fake-mode"), mode).unwrap();
    std::fs::write(dir.join(".fake-target"), schematic.display().to_string()).unwrap();
    Fixture { dir, schematic }
}

fn fake_install() -> KicadInstall {
    KicadInstall {
        cli: PathBuf::from(env!("CARGO_BIN_EXE_fake-kicad-cli")),
        version: KicadVersion::parse("10.0.7").unwrap(),
        source: InstallSource::Configured,
    }
}

fn config(f: &Fixture, cli_timeout: Duration) -> SessionConfig {
    SessionConfig {
        runtime_dir: f.dir.join("runtime"),
        cli_timeout,
        ..SessionConfig::default()
    }
}

async fn open(f: &Fixture) -> FileSession {
    open_with(f, Duration::from_secs(30)).await
}

async fn open_with(f: &Fixture, cli_timeout: Duration) -> FileSession {
    FileSession::open(
        &config(f, cli_timeout),
        &[fake_install()],
        OpenTarget::Existing(f.schematic.clone()),
    )
    .await
    .unwrap()
}

/// A plan that adds nothing but expects a net, which the fake `kicad-cli` never reports.
fn plan_expecting_a_net() -> OperationPlan {
    OperationPlan {
        circuit: "fault".into(),
        operations: vec![],
        expected_nets: vec![ExpectedNet {
            name: "N".into(),
            pins: BTreeSet::from(["R1.1".to_owned()]),
        }],
        references: BTreeMap::new(),
        findings: vec![],
        fallback_nets: vec![],
        readability: Readability::default(),
    }
}

/// A plan with nothing to verify.
fn empty_plan() -> OperationPlan {
    OperationPlan {
        expected_nets: vec![],
        ..plan_expecting_a_net()
    }
}

fn leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".thingsvu-"))
        .collect()
}

/// The shape every refused or failed apply has: nothing changed, nothing left over.
fn assert_untouched(f: &Fixture, before: &[u8]) {
    assert_eq!(std::fs::read(&f.schematic).unwrap(), before, "the schematic changed");
    assert_eq!(
        leftovers(&f.dir),
        Vec::<String>::new(),
        "a verification copy was left behind"
    );
}

#[tokio::test]
async fn a_crashing_kicad_cli_changes_nothing() {
    let f = fixture("crash", "crash");
    let session = open(&f).await;
    let before = std::fs::read(&f.schematic).unwrap();
    let fingerprint = session.fingerprint().await.unwrap();

    let error = session
        .apply_plan(&plan_expecting_a_net(), &fingerprint)
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::KicadError, "{error}");
    assert_eq!(error.mutation_state, MutationState::NotApplied);
    assert_untouched(&f, &before);

    // Reads that need the CLI fail cleanly too, without claiming anything.
    let read = session.netlist().await.unwrap_err();
    assert_eq!(read.code, ErrorCode::KicadError);
    assert_eq!(read.mutation_state, MutationState::NotStarted);
}

#[tokio::test]
async fn a_netlist_that_is_not_a_netlist_changes_nothing() {
    let f = fixture("garbage", "garbage");
    let session = open(&f).await;
    let before = std::fs::read(&f.schematic).unwrap();
    let fingerprint = session.fingerprint().await.unwrap();

    let error = session
        .apply_plan(&plan_expecting_a_net(), &fingerprint)
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::KicadError, "{error}");
    assert_eq!(error.mutation_state, MutationState::NotApplied);
    assert_untouched(&f, &before);
}

#[tokio::test]
async fn connectivity_that_differs_from_the_plan_is_never_written() {
    let f = fixture("mismatch", "ok");
    let session = open(&f).await;
    let before = std::fs::read(&f.schematic).unwrap();
    let fingerprint = session.fingerprint().await.unwrap();

    let error = session
        .apply_plan(&plan_expecting_a_net(), &fingerprint)
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::KicadError);
    assert_eq!(error.mutation_state, MutationState::NotApplied);
    assert!(error.message.contains("differs from the plan"), "{error}");
    assert!(error.message.contains("net N"), "names the net: {error}");
    assert_untouched(&f, &before);
}

#[tokio::test]
async fn a_hanging_kicad_cli_is_stopped_and_changes_nothing() {
    let f = fixture("hang", "hang");
    let session = open_with(&f, Duration::from_secs(1)).await;
    let before = std::fs::read(&f.schematic).unwrap();
    let fingerprint = session.fingerprint().await.unwrap();

    let started = Instant::now();
    let error = session
        .apply_plan(&plan_expecting_a_net(), &fingerprint)
        .await
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "did not wait for the hung process"
    );
    assert_eq!(error.mutation_state, MutationState::NotApplied, "{error}");
    assert_untouched(&f, &before);
}

#[tokio::test]
async fn a_file_changed_while_verifying_is_not_overwritten() {
    let f = fixture("race", "edit-original");
    let session = open(&f).await;
    let fingerprint = session.fingerprint().await.unwrap();

    let error = session.apply_plan(&empty_plan(), &fingerprint).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::FileChangedOnDisk, "{error}");
    assert_eq!(error.mutation_state, MutationState::NotApplied);
    // The outside edit is still there: it was not replaced by the older planned version.
    assert!(std::fs::read_to_string(&f.schematic)
        .unwrap()
        .contains("edited while the plan was being verified"));
    assert_eq!(leftovers(&f.dir), Vec::<String>::new());
}

#[tokio::test]
async fn a_plan_for_an_older_file_is_refused() {
    let f = fixture("stale", "ok");
    let session = open(&f).await;
    let fingerprint = session.fingerprint().await.unwrap();
    // Someone edits the file after the plan was made.
    let mut text = std::fs::read_to_string(&f.schematic).unwrap();
    text.push('\n');
    std::fs::write(&f.schematic, &text).unwrap();

    let error = session.apply_plan(&empty_plan(), &fingerprint).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::StalePlan);
    assert_eq!(error.mutation_state, MutationState::NotStarted);
    assert_untouched(&f, text.as_bytes());
}

#[tokio::test]
async fn an_editor_that_opens_the_file_blocks_changes() {
    let f = fixture("locked", "ok");
    let session = open(&f).await;
    let before = std::fs::read(&f.schematic).unwrap();
    let fingerprint = session.fingerprint().await.unwrap();
    std::fs::write(f.dir.join("~a.kicad_sch.lck"), "{}").unwrap();

    let error = session.apply_plan(&empty_plan(), &fingerprint).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::DocumentOpenElsewhere);
    assert_eq!(error.mutation_state, MutationState::NotStarted);
    assert_untouched(&f, &before);

    // A new session on a file that is open in an editor is refused outright.
    let refused = FileSession::open(
        &config(&f, Duration::from_secs(30)),
        &[fake_install()],
        OpenTarget::Existing(f.schematic.clone()),
    )
    .await
    .err()
    .expect("refused");
    assert_eq!(refused.code, ErrorCode::DocumentOpenElsewhere);
}

#[tokio::test]
async fn a_plan_with_errors_is_never_applied() {
    let f = fixture("errors", "ok");
    let session = open(&f).await;
    let before = std::fs::read(&f.schematic).unwrap();
    let fingerprint = session.fingerprint().await.unwrap();
    let mut plan = empty_plan();
    plan.findings.push(kicad_domain::Finding {
        severity: kicad_domain::Severity::Error,
        code: "TEST",
        message: "a blocking finding".into(),
        objects: vec![],
    });

    let error = session.apply_plan(&plan, &fingerprint).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert_untouched(&f, &before);
}

#[tokio::test]
async fn files_in_formats_it_was_not_checked_against_are_refused() {
    let f = fixture("formats", "ok");
    let valid = std::fs::read_to_string(&f.schematic).unwrap();
    let cases = [
        (valid.replace("20260306", "20250114"), "kicad-cli sch upgrade"),
        (valid.replace("20260306", "20300101"), "newer"),
        ("(kicad_pcb (version 20260101))".to_owned(), "not a schematic"),
        ("this is not an s-expression".to_owned(), "s-expression"),
    ];
    for (text, expected) in cases {
        std::fs::write(&f.schematic, &text).unwrap();
        let error = FileSession::open(
            &config(&f, Duration::from_secs(30)),
            &[fake_install()],
            OpenTarget::Existing(f.schematic.clone()),
        )
        .await
        .err()
        .unwrap_or_else(|| panic!("opened {expected}"));
        assert_eq!(error.code, ErrorCode::InvalidArgument, "{error}");
        assert!(error.message.contains(expected), "{expected}: {error}");
        assert_eq!(
            std::fs::read_to_string(&f.schematic).unwrap(),
            text,
            "refusing must not write"
        );
    }
}

#[tokio::test]
async fn only_old_leftover_copies_are_cleaned_up() {
    let f = fixture("cleanup", "ok");
    let old = f.dir.join(".thingsvu-old.kicad_sch");
    let fresh = f.dir.join(".thingsvu-fresh.kicad_sch");
    let unrelated = f.dir.join("keep.kicad_sch");
    for path in [&old, &fresh, &unrelated] {
        std::fs::write(path, "x").unwrap();
    }
    let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(hour_ago)
        .unwrap();

    let _session = open(&f).await;
    assert!(!old.exists(), "a copy an hour old is removed");
    assert!(fresh.exists(), "a copy that may be in use is kept");
    assert!(unrelated.exists(), "files that are not ours are never touched");
}
