use std::process::Command;

#[test]
fn startup_failure_reports_loading_without_polluting_piped_stdout() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_graft"))
        .args(["ui", "--no-open"])
        .arg(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("Opening Graft…\n"));
    assert!(stderr.contains("Error:"));
    assert!(!stderr.contains('\x1b'));
    assert!(!stderr.contains('\r'));
}
