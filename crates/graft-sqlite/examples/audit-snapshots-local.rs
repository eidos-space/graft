use graft_sqlite::repo_service::RepositoryCommandService;
use serde_json::{Value, json};
use std::{fs, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = PathBuf::from(std::env::args().nth(1).ok_or("audit directory required")?);
    let mut service = RepositoryCommandService::open(&base.join("space/.audit.sqlite"))?;
    let plan: Vec<Value> = serde_json::from_slice(&fs::read(base.join("plan.json"))?)?;
    let output = base.join("snapshot.sqlite");
    let mut results = Vec::new();
    for mut entry in plan {
        if output.exists() {
            fs::remove_file(&output)?;
        }
        match service.export_revision_sqlite_path(
            entry["first_commit"].as_str().unwrap(),
            &PathBuf::from(entry["path"].as_str().unwrap()),
            &output,
        ) {
            Err(error) => {
                entry["result"] = json!("unreadable_locally");
                entry["error"] = json!(error.to_string());
            }
            Ok(_) => {
                let db = rusqlite::Connection::open_with_flags(
                    &output,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )?;
                let check = db
                    .prepare("PRAGMA integrity_check")?
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                entry["result"] = json!(if check == ["ok"] {
                    "ok"
                } else {
                    "sqlite_integrity_error"
                });
                if check != ["ok"] {
                    entry["error"] = json!(check);
                }
            }
        }
        let bad = entry["result"] != "ok";
        results.push(entry);
        fs::write(
            base.join("snapshots.json"),
            serde_json::to_vec_pretty(&results)?,
        )?;
        if bad || results.len() % 20 == 0 {
            eprintln!(
                "scanned {}; issues {}",
                results.len(),
                results.iter().filter(|v| v["result"] != "ok").count()
            );
        }
    }
    if output.exists() {
        fs::remove_file(output)?;
    }
    Ok(())
}
