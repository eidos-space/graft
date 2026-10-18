use graft::repo::object::{BlobObject, Object, ObjectId};
use serde_json::json;
use std::{collections::BTreeMap, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::args().nth(1).ok_or("repository path required")?);
    let mut objects = BTreeMap::new();
    let mut issues = Vec::new();
    let mut counts = BTreeMap::<String, usize>::new();
    for directory in fs::read_dir(root.join(".graft/objects"))? {
        let directory = directory?;
        if !directory.file_type()?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(directory.path())? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let id = format!(
                "{}{}",
                directory.file_name().to_string_lossy(),
                entry.file_name().to_string_lossy()
            );
            if id.len() != 64 {
                continue;
            }
            let bytes = fs::read(entry.path())?;
            if ObjectId::for_bytes(&bytes).to_string() != id {
                issues.push(json!({"object": id, "error": "content hash mismatch"}));
            }
            match Object::decode(&bytes) {
                Ok(object) => {
                    *counts.entry(object.kind().to_string()).or_default() += 1;
                    objects.insert(id, object);
                }
                Err(error) => issues.push(json!({"object": id, "error": error.to_string()})),
            }
        }
    }
    let mut commits = Vec::new();
    let mut snapshots = BTreeMap::<String, serde_json::Value>::new();
    for (id, object) in &objects {
        let references = match object {
            Object::Commit(commit) => {
                let mut refs = commit
                    .parents
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();
                refs.push(commit.tree.to_string());
                commits.push(json!({"id": id, "tree": commit.tree.to_string(), "parents": commit.parents.iter().map(ToString::to_string).collect::<Vec<_>>(), "timestamp_ms": commit.committer.timestamp_ms, "message": commit.message}));
                if let Some(Object::Tree(tree)) = objects.get(&commit.tree.to_string()) {
                    for entry in &tree.entries {
                        if let Some(Object::Blob(BlobObject::SqliteSnapshot(snapshot))) =
                            objects.get(&entry.oid.to_string())
                        {
                            snapshots.entry(entry.oid.to_string()).or_insert_with(|| json!({"commit": id, "path": entry.path, "volume": snapshot.volume.to_string(), "page_count": snapshot.page_count, "ranges": snapshot.ranges.iter().map(|range| json!({"log": range.log.to_string(), "start": range.start, "end": range.end, "commits": range.commits.iter().map(|c| json!({"lsn": c.lsn, "hash": c.commit_hash.to_string()})).collect::<Vec<_>>()})).collect::<Vec<_>>()}));
                        }
                    }
                }
                refs
            }
            Object::Tree(tree) => tree
                .entries
                .iter()
                .map(|entry| entry.oid.to_string())
                .collect(),
            Object::Tag(tag) => vec![tag.object.to_string()],
            Object::Blob(_) => Vec::new(),
        };
        for reference in references {
            if !objects.contains_key(&reference) {
                issues.push(json!({"object": id, "missing_reference": reference}));
            }
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"counts": counts, "issues": issues, "commits": commits, "sqlite_snapshots": snapshots})
        )?
    );
    Ok(())
}
