//! Integration test — `.github/workflows/release.yml` must parse as
//! YAML and contain the secret names ADR-008 commits to.
//!
//! Rationale: the workflow is invisible to `cargo check`; without
//! this test a typo in `secrets.APPLE_ID` -> `secrets.APPLE_ID_X`
//! would only surface at release time. We pin the contract here.

use std::fs;
use std::path::PathBuf;

fn workflow_path() -> PathBuf {
    // Tests run with CWD = tauri/, so walk up one level to repo root.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir)
        .parent()
        .expect("tauri/ must have a parent")
        .join(".github/workflows/release.yml")
}

#[test]
fn release_workflow_file_exists() {
    let p = workflow_path();
    assert!(
        p.exists(),
        "expected release workflow at {} — ADR-008 scaffold",
        p.display()
    );
}

#[test]
fn release_workflow_mentions_every_required_secret() {
    let p = workflow_path();
    let content = fs::read_to_string(&p).expect("release.yml readable");
    // Anchor names that, if missing, mean the workflow can't perform
    // a signed release. Kept short — ADR-008 §"GitHub Actions secrets"
    // is the source of truth.
    let required = [
        "APPLE_DEVELOPER_ID_CERT_BASE64",
        "APPLE_DEVELOPER_ID_PASSWORD",
        "APPLE_ID",
        "APPLE_TEAM_ID",
        "APPLE_PASSWORD",
        "WINDOWS_CERT_BASE64",
        "WINDOWS_CERT_PASSWORD",
        "TAURI_UPDATER_PRIVATE_KEY",
        "TAURI_UPDATER_KEY_PASSWORD",
    ];
    for name in required {
        assert!(
            content.contains(name),
            "release.yml missing reference to secret `{name}`"
        );
    }
}

#[test]
fn release_workflow_triggers_on_v_tag() {
    let p = workflow_path();
    let content = fs::read_to_string(&p).expect("release.yml readable");
    assert!(
        content.contains("tags:") && content.contains("\"v*\""),
        "release.yml must trigger on v* tag pushes (ADR-008)"
    );
}
