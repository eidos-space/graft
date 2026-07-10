use std::{fs, process::Command};

use graft::repo::Repository;

#[test]
fn diff_content_cli_returns_bounded_read_only_json() {
    let temp_dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp_dir.path()).unwrap();
    let note = temp_dir.path().join("note.md");

    fs::write(&note, "# Before\n").unwrap();
    repo.stage_artifact_path(&note).unwrap();
    repo.commit_staged("before").unwrap();
    fs::write(&note, "# After\n").unwrap();
    repo.stage_artifact_path(&note).unwrap();
    repo.commit_staged("after").unwrap();
    let status_before = repo.status().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(temp_dir.path())
        .args([
            "diff",
            "--json",
            "--content",
            "--max-content-bytes",
            "128",
            "HEAD~1",
            "HEAD",
            "--",
            "note.md",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["content"]["path"], "note.md");
    assert_eq!(json["content"]["before"]["state"], "utf8");
    assert_eq!(json["content"]["before"]["content"], "# Before\n");
    assert_eq!(json["content"]["before"]["size"], 9);
    assert!(json["content"]["before"]["content_hash"].is_string());
    assert_eq!(json["content"]["after"]["state"], "utf8");
    assert_eq!(json["content"]["after"]["content"], "# After\n");
    assert_eq!(json["content"]["after"]["size"], 8);
    assert!(json["content"]["after"]["content_hash"].is_string());

    let status_after = repo.status().unwrap();
    assert_eq!(status_after.head_target, status_before.head_target);
    assert_eq!(status_after.staged, status_before.staged);
    assert_eq!(status_after.unstaged, status_before.unstaged);
    assert_eq!(fs::read_to_string(note).unwrap(), "# After\n");
}
