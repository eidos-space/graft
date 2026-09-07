use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use std::io::Read;

const MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct PathHistoryOptions {
    pub path: String,
    pub limit: usize,
    pub max_commits: usize,
    pub max_bytes: u64,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathHistoryEntry {
    pub id: String,
    pub parents: Vec<String>,
    pub message: String,
    pub timestamp_ms: u64,
    pub change: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PathHistoryTelemetry {
    pub commits_scanned: usize,
    pub commit_objects_read: usize,
    pub tree_objects_read: usize,
    pub object_bytes_read: u64,
    pub blob_objects_read: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathHistoryPage {
    pub path: String,
    pub start: Option<String>,
    pub commits: Vec<PathHistoryEntry>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub telemetry: PathHistoryTelemetry,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    path: String,
    start: String,
    next: String,
}

type PathIdentity = Option<(object::TreeEntryMode, object::ObjectId)>;

impl Repository {
    /// Exact-path first-parent history, pinned to the initial HEAD. Reads metadata only.
    pub fn path_history(&self, options: &PathHistoryOptions) -> Result<PathHistoryPage> {
        cancellation_checkpoint()?;
        if !(1..=100).contains(&options.limit)
            || !(1..=1000).contains(&options.max_commits)
            || !(1024..=MAX_BYTES).contains(&options.max_bytes)
        {
            return Err(invalid(
                "limit 1..100, maxCommits 1..1000, maxBytes 1024..67108864",
            ));
        }
        let path = normalize_repo_path_key(&options.path)?;
        let (start, mut next) = self.path_history_start(&path, options.cursor.as_deref())?;
        let mut page = PathHistoryPage {
            path: path.clone(),
            start,
            commits: Vec::new(),
            has_more: false,
            next_cursor: None,
            telemetry: PathHistoryTelemetry::default(),
        };
        // Retain only one tree's selected entry, never a repository-wide tree cache.
        let mut cache = None;
        while let Some(id) = next.as_ref() {
            cancellation_checkpoint()?;
            if page.telemetry.commits_scanned == options.max_commits
                || page.commits.len() == options.limit
            {
                break;
            }
            let Some((entry, parent)) = self.history_comparison(
                id,
                &path,
                options.max_bytes,
                &mut page.telemetry,
                &mut cache,
            )?
            else {
                break;
            };
            if let Some(entry) = entry {
                page.commits.push(entry);
            }
            page.telemetry.commits_scanned += 1;
            next = parent;
        }
        if next.is_some() && page.telemetry.commits_scanned == 0 {
            return Err(invalid(
                "maxBytes cannot fit one comparison; increase maxBytes (maximum 64 MiB)",
            ));
        }
        page.has_more = next.is_some();
        if let Some(next) = next {
            let cursor = Cursor {
                version: 1,
                path,
                start: page.start.clone().expect("nonempty history has start"),
                next,
            };
            let bytes = serde_json::to_vec(&cursor).map_err(|_| invalid("cannot encode cursor"))?;
            page.next_cursor = Some(format!(
                "{}.{}",
                URL_SAFE_NO_PAD.encode(&bytes),
                object::ObjectId::for_bytes(&bytes)
            ));
        }
        cancellation_checkpoint()?;
        Ok(page)
    }

    fn history_comparison(
        &self,
        id: &str,
        path: &str,
        budget: u64,
        telemetry: &mut PathHistoryTelemetry,
        cache: &mut Option<(object::ObjectId, PathIdentity)>,
    ) -> Result<Option<(Option<PathHistoryEntry>, Option<String>)>> {
        let Some(commit) = self.history_commit(id, budget, telemetry)? else {
            return Ok(None);
        };
        let Some(after) = self.history_identity(&commit.tree, path, budget, telemetry, cache)?
        else {
            return Ok(None);
        };
        let parent = commit.parents.first().map(ToString::to_string);
        let before = if let Some(parent) = &parent {
            let Some(parent_commit) = self.history_commit(parent, budget, telemetry)? else {
                return Ok(None);
            };
            let Some(before) =
                self.history_identity(&parent_commit.tree, path, budget, telemetry, cache)?
            else {
                return Ok(None);
            };
            before
        } else {
            None
        };
        let entry = (before != after).then(|| PathHistoryEntry {
            id: id.into(),
            parents: commit.parents.iter().map(ToString::to_string).collect(),
            message: commit.message,
            timestamp_ms: commit.committer.timestamp_ms,
            change: if before.is_none() {
                "added"
            } else if after.is_none() {
                "deleted"
            } else {
                "modified"
            }
            .into(),
        });
        Ok(Some((entry, parent)))
    }

    fn path_history_start(
        &self,
        path: &str,
        cursor: Option<&str>,
    ) -> Result<(Option<String>, Option<String>)> {
        let Some(cursor) = cursor else {
            let head = self.head_target()?;
            return Ok((head.clone(), head));
        };
        if cursor.len() > 16384 {
            return Err(invalid("cursor too large"));
        }
        let (payload, checksum) = cursor
            .split_once('.')
            .ok_or_else(|| invalid("invalid cursor"))?;
        let bytes = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| invalid("invalid cursor"))?;
        if object::ObjectId::for_bytes(&bytes).as_str() != checksum {
            return Err(invalid("cursor checksum mismatch"));
        }
        let cursor: Cursor =
            serde_json::from_slice(&bytes).map_err(|_| invalid("invalid cursor"))?;
        if cursor.version != 1 || cursor.path != path {
            return Err(invalid("cursor version or path mismatch"));
        }
        object::ObjectId::from_str(&cursor.start)?;
        object::ObjectId::from_str(&cursor.next)?;
        Ok((Some(cursor.start), Some(cursor.next)))
    }

    fn history_commit(
        &self,
        id: &str,
        budget: u64,
        telemetry: &mut PathHistoryTelemetry,
    ) -> Result<Option<object::CommitObject>> {
        let id = object::ObjectId::from_str(id)?;
        let Some(value) = self.history_object(&id, budget, telemetry)? else {
            return Ok(None);
        };
        telemetry.commit_objects_read += 1;
        match value {
            object::Object::Commit(commit) => Ok(Some(commit)),
            _ => Err(invalid("cursor does not name a commit")),
        }
    }

    fn history_identity(
        &self,
        tree: &object::ObjectId,
        path: &str,
        budget: u64,
        telemetry: &mut PathHistoryTelemetry,
        cache: &mut Option<(object::ObjectId, PathIdentity)>,
    ) -> Result<Option<PathIdentity>> {
        if let Some((id, entry)) = cache.as_ref()
            && id == tree
        {
            return Ok(Some(entry.clone()));
        }
        let Some(value) = self.history_object(tree, budget, telemetry)? else {
            return Ok(None);
        };
        telemetry.tree_objects_read += 1;
        let object::Object::Tree(value) = value else {
            return Err(invalid("expected tree object"));
        };
        let identity = value
            .entries
            .binary_search_by(|entry| entry.path.as_str().cmp(path))
            .ok()
            .map(|i| (value.entries[i].mode, value.entries[i].oid.clone()));
        *cache = Some((tree.clone(), identity.clone()));
        Ok(Some(identity))
    }

    fn history_object(
        &self,
        id: &object::ObjectId,
        budget: u64,
        telemetry: &mut PathHistoryTelemetry,
    ) -> Result<Option<object::Object>> {
        cancellation_checkpoint()?;
        let remaining = budget - telemetry.object_bytes_read;
        let file = fs::File::open(self.object_store().path_for(id))?;
        if file.metadata()?.len() > remaining {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        // Defend against a file growing between stat and read, and check cancellation per 64 KiB.
        let mut reader = file.take(remaining);
        let mut buffer = [0_u8; 65536];
        loop {
            cancellation_checkpoint()?;
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            telemetry.object_bytes_read += count as u64;
            bytes.extend_from_slice(&buffer[..count]);
        }
        let actual = object::ObjectId::for_bytes(&bytes);
        if &actual != id {
            return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                expected: id.clone(),
                actual,
            }));
        }
        let value = object::Object::decode(&bytes)?;
        cancellation_checkpoint()?;
        Ok(Some(value))
    }
}

fn invalid(message: &str) -> RepoErr {
    RepoErr::InvalidPathHistory(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(
        repo: &Repository,
        paths: &[(&str, &str)],
        parents: &[String],
        message: &str,
        time: u64,
    ) -> String {
        let entries = paths
            .iter()
            .map(|(path, content)| object::TreeEntry {
                path: (*path).into(),
                mode: object::TreeEntryMode::Regular,
                // Deliberately no payload: history must work without any blobs.
                oid: object::ObjectId::for_bytes(content.as_bytes()),
            })
            .collect();
        let tree = object::TreeObject::new(entries).unwrap();
        let tree = repo
            .object_store()
            .write(&object::Object::Tree(tree))
            .unwrap();
        let value = repo
            .canonical_commit_object(tree, parents, message, time, vec![], None)
            .unwrap();
        let id = repo
            .object_store()
            .write(&object::Object::Commit(value))
            .unwrap()
            .to_string();
        repo.write_head_with_message(&Head::Detached { commit: id.clone() }, "fixture")
            .unwrap();
        id
    }

    fn options(path: &str) -> PathHistoryOptions {
        PathHistoryOptions {
            path: path.into(),
            limit: 100,
            max_commits: 100,
            max_bytes: 8 * 1024 * 1024,
            cursor: None,
        }
    }

    #[test]
    fn path_history_first_parent_renames_deletion_recreation_and_pinned_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        assert!(repo.path_history(&options("a")).unwrap().commits.is_empty());
        let root = commit(&repo, &[("a", "one")], &[], "root", 100);
        let renamed = commit(
            &repo,
            &[("b", "one")],
            std::slice::from_ref(&root),
            "rename",
            80,
        );
        let rebuilt = commit(
            &repo,
            &[("a", "two"), ("b", "one")],
            std::slice::from_ref(&renamed),
            "rebuild",
            60,
        );
        let side = commit(
            &repo,
            &[("a", "side")],
            std::slice::from_ref(&root),
            "side",
            200,
        );
        let merged = commit(
            &repo,
            &[("a", "merged")],
            &[rebuilt.clone(), side],
            "merge",
            40,
        );
        let mut query = options("a");
        query.limit = 1;
        let first = repo.path_history(&query).unwrap();
        assert_eq!(first.commits[0].id, merged);
        assert_eq!(first.commits[0].parents.len(), 2);
        commit(&repo, &[], &[], "unrelated HEAD", 999);
        query.cursor = first.next_cursor;
        let mut ids = vec![];
        let mut changes = vec![];
        loop {
            let page = repo.path_history(&query).unwrap();
            assert_eq!(page.start.as_deref(), Some(merged.as_str()));
            for entry in page.commits {
                ids.push(entry.id);
                changes.push(entry.change);
            }
            query.cursor = page.next_cursor;
            if !page.has_more {
                break;
            }
        }
        assert_eq!(ids, [rebuilt, renamed, root]);
        assert_eq!(changes, ["added", "deleted", "added"]);
        query.path = "b".into();
        query.cursor = Some("broken".into());
        assert!(repo.path_history(&query).is_err());
    }

    #[test]
    fn path_history_bounds_sparse_scan_bytes_and_cancellation_without_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let mut head = commit(&repo, &[("rare", "one")], &[], "root", 0);
        for i in 0..30 {
            head = commit(
                &repo,
                &[("rare", "one"), ("other", &i.to_string())],
                &[head],
                "other",
                i,
            );
        }
        let mut query = options("rare");
        query.max_commits = 5;
        let page = repo.path_history(&query).unwrap();
        assert!(page.commits.is_empty());
        assert!(page.has_more);
        assert_eq!(page.telemetry.commits_scanned, 5);
        assert_eq!(page.telemetry.blob_objects_read, 0);
        assert_eq!(page.telemetry.tree_objects_read, 6);
        assert_eq!(page.telemetry.commit_objects_read, 10);
        query.max_commits = 100;
        query.max_bytes = 2048;
        let page = repo.path_history(&query).unwrap();
        assert!(page.telemetry.commits_scanned < 30);
        assert!(page.telemetry.object_bytes_read <= 2048);
        assert!(page.has_more);
        let token = CancellationToken::new();
        token.cancel();
        assert!(matches!(
            with_cancellation(&token, || repo.path_history(&query)),
            Err(RepoErr::Cancelled)
        ));
        query.max_bytes = 1024;
        let names: Vec<_> = (0..100).map(|i| format!("path-{i}")).collect();
        let paths: Vec<_> = names.iter().map(|p| (p.as_str(), "x")).collect();
        commit(&repo, &paths, &[head], "big", 40);
        assert!(matches!(
            repo.path_history(&query),
            Err(RepoErr::InvalidPathHistory(_))
        ));
    }
}
