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
fn status_suggests_cli_add_commands() {
    let temp_dir = tempfile::tempdir().unwrap();
    Repository::init(temp_dir.path()).unwrap();
    fs::write(temp_dir.path().join("note.md"), "untracked\n").unwrap();

    let status = run_graft(temp_dir.path(), &["status"]);

    assert!(
        status.contains("use 'graft add <path>' or 'graft add --all' to stage"),
        "{status}"
    );
    assert!(!status.contains("pragma graft_add"), "{status}");
}

#[test]
fn conflicts_suggests_cli_resolve_commands() {
    let temp_dir = tempfile::tempdir().unwrap();
    Repository::init(temp_dir.path()).unwrap();
    let note = temp_dir.path().join("note.md");

    fs::write(&note, "base\n").unwrap();
    run_graft(temp_dir.path(), &["add", "note.md"]);
    run_graft(temp_dir.path(), &["commit", "-m", "base"]);

    run_graft(temp_dir.path(), &["switch", "-c", "feature"]);
    fs::write(&note, "feature\n").unwrap();
    run_graft(temp_dir.path(), &["add", "note.md"]);
    run_graft(temp_dir.path(), &["commit", "-m", "feature"]);

    run_graft(temp_dir.path(), &["switch", "main"]);
    fs::write(&note, "main\n").unwrap();
    run_graft(temp_dir.path(), &["add", "note.md"]);
    run_graft(temp_dir.path(), &["commit", "-m", "main"]);
    run_graft(temp_dir.path(), &["merge", "feature"]);

    let conflicts = run_graft(temp_dir.path(), &["conflicts"]);

    assert!(
        conflicts.contains("graft resolve --ours <path>"),
        "{conflicts}"
    );
    assert!(
        conflicts.contains("graft resolve --theirs <path>"),
        "{conflicts}"
    );
    assert!(
        conflicts.contains("graft resolve --manual <path>"),
        "{conflicts}"
    );
    assert!(!conflicts.contains("pragma graft_resolve"), "{conflicts}");
}
