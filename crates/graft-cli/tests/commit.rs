use std::{fs, process::Command};

use graft::repo::Repository;

#[test]
fn empty_commit_reports_a_clean_cli_error() {
    let temp_dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp_dir.path()).unwrap();
    let note = temp_dir.path().join("note.md");
    fs::write(&note, "tracked\n").unwrap();
    repo.stage_artifact_path(&note).unwrap();
    repo.commit_staged("initial").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(temp_dir.path())
        .args(["commit", "-m", "empty"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Error: no changes added to commit"),
        "{stderr}"
    );
    assert!(!stderr.contains("unknown error"), "{stderr}");
    assert!(!stderr.contains("[graft:commit:"), "{stderr}");
}
