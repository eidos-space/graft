use std::{fs, path::Path, process::Command};

use graft::repo::Repository;

fn run_graft(worktree: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(worktree)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "graft {} failed:\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn log_uses_git_style_blocks_and_keeps_newest_commit_first() {
    let temp_dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp_dir.path()).unwrap();
    let note = temp_dir.path().join("note.md");

    fs::write(&note, "first\n").unwrap();
    repo.stage_artifact_path(&note).unwrap();
    let first = repo.commit_staged("first commit").unwrap();
    fs::write(&note, "second\n").unwrap();
    repo.stage_artifact_path(&note).unwrap();
    let second = repo.commit_staged("second commit\nwith details").unwrap();

    let output = run_graft(temp_dir.path(), &["log"]);

    let newest = output.find(&format!("commit {}", second.id)).unwrap();
    let oldest = output.find(&format!("commit {}", first.id)).unwrap();
    assert!(newest < oldest, "{output}");
    assert!(
        output.contains("Author: Graft <graft@example.invalid>"),
        "{output}"
    );
    assert!(output.contains("Date:   "), "{output}");
    assert!(
        output.contains("    second commit\n    with details"),
        "{output}"
    );
    assert!(!output.contains("\nparent "), "{output}");
}
