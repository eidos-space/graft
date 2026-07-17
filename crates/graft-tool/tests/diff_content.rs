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

#[test]
fn diff_content_cli_preserves_utf8_with_ascii_percent() {
    let temp_dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp_dir.path()).unwrap();
    let note = temp_dir.path().join("note.md");
    let before = "# 债券追踪\nSpaceX 债券价格跌破发行价 10% 后仍需观察\n";
    let after = "# 债券追踪\nSpaceX 债券价格跌破发行价 15% 后已企稳\n";

    fs::write(&note, before).unwrap();
    repo.stage_artifact_path(&note).unwrap();
    repo.commit_staged("before").unwrap();
    fs::write(&note, after).unwrap();
    repo.stage_artifact_path(&note).unwrap();
    repo.commit_staged("after").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(temp_dir.path())
        .args([
            "diff",
            "--json",
            "--content",
            "--max-content-bytes",
            "1048576",
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
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "graft diff should return complete JSON for content containing `%`: {err}; stdout={}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(json["content"]["before"]["content"], before);
    assert_eq!(json["content"]["after"]["content"], after);
}

#[test]
fn diff_content_cli_compares_a_revision_to_the_worktree() {
    let temp_dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp_dir.path()).unwrap();
    let note = temp_dir.path().join("note.md");

    fs::write(&note, "# Committed\n").unwrap();
    repo.stage_artifact_path(&note).unwrap();
    repo.commit_staged("committed").unwrap();
    fs::write(&note, "# Working tree\n").unwrap();
    let status_before = repo.status().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(temp_dir.path())
        .args(["diff", "--json", "--content", "HEAD", "--", "note.md"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["from"], status_before.head_target.unwrap());
    assert_eq!(json["to"], "worktree");
    assert_eq!(json["content"]["path"], "note.md");
    assert_eq!(json["content"]["before"]["content"], "# Committed\n");
    assert_eq!(json["content"]["after"]["content"], "# Working tree\n");

    let status_after = repo.status().unwrap();
    assert_eq!(status_after.staged, status_before.staged);
    assert_eq!(status_after.unstaged, status_before.unstaged);
    assert_eq!(fs::read_to_string(note).unwrap(), "# Working tree\n");
}

#[test]
fn root_diff_cli_reports_first_commit_metadata_and_content() {
    let temp_dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp_dir.path()).unwrap();
    let note = temp_dir.path().join("first.md");
    let binary = temp_dir.path().join("image.bin");
    fs::write(&note, "# First\n").unwrap();
    fs::write(&binary, [0, 159, 146, 150]).unwrap();
    repo.stage_artifact_path(&note).unwrap();
    repo.stage_artifact_path(&binary).unwrap();
    let first = repo.commit_staged("first").unwrap();
    let status_before = repo.status().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(temp_dir.path())
        .args(["diff", "--json", "--root", &first.id])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["from"], "root");
    assert_eq!(json["to"], first.id);
    assert_eq!(json["paths"][0]["path"], "first.md");
    assert_eq!(json["paths"][0]["change"], "added");
    assert_eq!(json["paths"][0]["kind"], "text_file");
    assert_eq!(json["paths"][1]["path"], "image.bin");
    assert_eq!(json["paths"][1]["change"], "added");
    assert_eq!(json["paths"][1]["kind"], "binary_file");

    let output = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(temp_dir.path())
        .args([
            "diff",
            "--json",
            "--content",
            "--max-content-bytes",
            "128",
            "--root",
            &first.id,
            "--",
            "first.md",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["content"]["path"], "first.md");
    assert_eq!(json["content"]["before"]["state"], "absent");
    assert_eq!(json["content"]["after"]["state"], "utf8");
    assert_eq!(json["content"]["after"]["content"], "# First\n");
    let status_after = repo.status().unwrap();
    assert_eq!(status_after.head_target, status_before.head_target);
    assert_eq!(status_after.staged, status_before.staged);
    assert_eq!(status_after.unstaged, status_before.unstaged);
    assert_eq!(status_after.conflicted, status_before.conflicted);
}

#[cfg(not(windows))]
#[test]
fn diff_content_cli_preserves_quotes_and_repeated_whitespace_in_path() {
    let temp_dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp_dir.path()).unwrap();
    let relative = "notes/it's  \"quoted\".md";
    let note = temp_dir.path().join(relative);
    fs::create_dir_all(note.parent().unwrap()).unwrap();

    fs::write(&note, "# Before\n").unwrap();
    repo.stage_artifact_path(&note).unwrap();
    repo.commit_staged("before").unwrap();
    fs::write(&note, "# After\n").unwrap();
    repo.stage_artifact_path(&note).unwrap();
    repo.commit_staged("after").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(temp_dir.path())
        .args([
            "diff",
            "--json",
            "--content",
            "HEAD~1",
            "HEAD",
            "--",
            relative,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["content"]["path"], relative);
    assert_eq!(json["content"]["before"]["content"], "# Before\n");
    assert_eq!(json["content"]["after"]["content"], "# After\n");
}

#[cfg(not(windows))]
#[test]
fn add_cli_preserves_quotes_and_repeated_whitespace_in_path() {
    let temp_dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp_dir.path()).unwrap();
    let relative = "notes/it's  \"quoted\".md";
    let note = temp_dir.path().join(relative);
    fs::create_dir_all(note.parent().unwrap()).unwrap();
    fs::write(&note, "# Draft\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(temp_dir.path())
        .args(["add", "--json", "--", relative])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["paths"][0]["path"], relative);
    let status = repo.status().unwrap();
    assert_eq!(status.staged[0], relative);
}

#[cfg(not(windows))]
#[test]
fn restore_cli_preserves_quotes_and_repeated_whitespace_in_path() {
    let temp_dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp_dir.path()).unwrap();
    let relative = "notes/it's  \"quoted\".md";
    let note = temp_dir.path().join(relative);
    fs::create_dir_all(note.parent().unwrap()).unwrap();

    fs::write(&note, "before\n").unwrap();
    repo.stage_artifact_path(&note).unwrap();
    repo.commit_staged("before").unwrap();
    fs::write(&note, "after\n").unwrap();
    repo.stage_artifact_path(&note).unwrap();
    repo.commit_staged("after").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(temp_dir.path())
        .args(["restore", "--json", "--source", "HEAD~1", "--", relative])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["path"], relative);
    assert!(json.get("paths").is_none());
    assert_eq!(fs::read_to_string(note).unwrap(), "before\n");
}
