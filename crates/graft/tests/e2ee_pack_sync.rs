#![cfg(all(feature = "e2ee", unix))]

use graft::{
    e2ee::{EncryptedConfig, RepositoryId, RootKey, SecretSession, SessionBinding},
    remote::{EncryptedTransferBudget, RemoteConfig, RemoteCredentials},
    repo::{
        Repository,
        object::{CommitObject, Object, Signature, TreeObject},
    },
};
use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Fixture {
    directory: tempfile::TempDir,
    server: Child,
    identity: RepositoryId,
    config: EncryptedConfig,
    remote: RemoteConfig,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

impl Fixture {
    fn new() -> Result<Self> {
        Self::with_fetch_fault(None)
    }

    fn with_fetch_fault(fetch_fault: Option<&str>) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let identity = RepositoryId::generate()?;
        let root = RootKey::import(identity, zeroize::Zeroizing::new([42; 32]));
        let config = EncryptedConfig::create(&root, b"synthetic pack regression password")?;
        let hex = identity
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let uuid = format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        );
        let fixture = directory.path().join("fixture.json");
        fs::write(
            &fixture,
            serde_json::to_vec(&serde_json::json!({
                "encryption": {"profile":"encrypted-remote-v1", "version":1, "suite":1, "uuid":uuid},
                "objects": [["e2ee/config", config.as_bytes()]],
                "fetchFault": fetch_fault,
                "receiveFault": fetch_fault,
                "omitPackListings": fetch_fault == Some("receive-collision"),
                "enableMultipart": fetch_fault == Some("multipart-file"),
                "maxRequestBytes": if fetch_fault == Some("multipart-file") { 1024 * 1024 } else { 64 * 1024 * 1024 },
                "multipartPartBytes": 256 * 1024,
            }))?,
        )?;
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut server = Command::new("node")
            .arg(workspace.join("tests/e2ee/remote-service.mjs"))
            .arg(fixture)
            .arg(directory.path().join("capture.json"))
            .stdout(Stdio::piped())
            .spawn()?;
        let mut line = String::new();
        BufReader::new(server.stdout.take().ok_or("missing server stdout")?)
            .read_line(&mut line)?;
        let ready: serde_json::Value = serde_json::from_str(&line)?;
        let url = ready["url"]
            .as_str()
            .ok_or("server did not return URL")?
            .to_owned();
        Ok(Self {
            directory,
            server,
            identity,
            config,
            remote: RemoteConfig::EncryptedHttp { url, uuid },
        })
    }

    fn repo(&self, name: &str, budget: usize) -> Result<Repository> {
        Ok(self.repo_session(name, budget)?.0)
    }

    fn repo_session(
        &self,
        name: &str,
        budget: usize,
    ) -> Result<(Repository, SecretSession, RemoteCredentials)> {
        let private = self.directory.path().join(format!("private-{name}"));
        fs::create_dir(&private)?;
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700))?;
        let session = SecretSession::new(
            SessionBinding::new(
                "pack-regression.test".into(),
                "synthetic-account".into(),
                self.identity,
            )?,
            None,
        )?;
        session.unlock_with_root(
            &self.config,
            RootKey::import(self.identity, zeroize::Zeroizing::new([42; 32])),
        )?;
        let credentials = RemoteCredentials::explicit();
        credentials.set_http_bearer_token("origin", "account-token".into())?;
        let journals_directory = self.directory.path().join(format!("journals-{name}"));
        fs::create_dir(&journals_directory)?;
        fs::set_permissions(&journals_directory, fs::Permissions::from_mode(0o700))?;
        let journals = graft::e2ee::FileRefMutationJournalProvider::create(
            &journals_directory,
            session.binding(),
        )?;
        let uploads_directory = self.directory.path().join(format!("uploads-{name}"));
        fs::create_dir(&uploads_directory)?;
        fs::set_permissions(&uploads_directory, fs::Permissions::from_mode(0o700))?;
        let uploads = graft::e2ee::FileMultipartJournalProvider::create(
            &uploads_directory,
            session.binding(),
        )?;
        credentials.set_encrypted_session_with_recovery(
            "origin",
            &self.remote,
            session.clone(),
            EncryptedTransferBudget {
                private_directory: private,
                max_buffered_plaintext: budget,
                max_ciphertext_spool: 64 * 1024 * 1024,
            },
            std::sync::Arc::new(journals),
            std::sync::Arc::new(uploads),
        )?;
        let repo = Repository::init(self.directory.path().join(name))?
            .with_remote_credentials(credentials.clone());
        repo.remote_add("origin", self.remote.clone())?;
        Ok((repo, session, credentials))
    }

    fn capture(&self) -> Result<serde_json::Value> {
        Ok(serde_json::from_slice(&fs::read(
            self.directory.path().join("capture.json"),
        )?)?)
    }
}

#[test]
#[ignore = "requires Node.js, built @eidos.space/graft-remote, and a loopback listener"]
fn encrypted_small_payload_failure_and_cancellation_do_not_publish_fetch_state() -> Result<()> {
    let fixture = Fixture::with_fetch_fault(Some("small-payload-missing"))?;
    let source = fixture.repo("payload-source", 1024 * 1024)?;
    let expected = [
        ("small.bin", vec![0x80_u8; 128 * 1024]),
        ("stream.bin", vec![0x90_u8; 300 * 1024]),
    ];
    for (name, bytes) in &expected {
        let path = source.worktree().join(name);
        fs::write(&path, bytes)?;
        source.stage_artifact_path(&path)?;
    }
    let head = source.commit_staged("small external payloads")?.id;
    source.push("origin", "main")?;
    let target = fixture.repo("payload-target", 1024 * 1024)?;
    let cancellation = graft::repo::CancellationToken::new();
    cancellation.cancel();
    assert!(
        graft::repo::with_cancellation(&cancellation, || target.fetch_for_clone("origin", "main"))
            .is_err()
    );
    assert_eq!(target.remote_tracking_ref("origin", "main")?, None);
    assert!(target.fetch_for_clone("origin", "main").is_err());
    assert_eq!(target.remote_tracking_ref("origin", "main")?, None);
    assert!(
        fixture
            .capture()?
            .as_array()
            .unwrap()
            .iter()
            .any(|request| request["droppedSmallPayload"] == true)
    );
    assert_eq!(target.fetch_for_clone("origin", "main")?.fetch.head, head);
    for (name, bytes) in expected {
        let path = target.worktree().join(name);
        let checkout = target.checkout_artifact_from_revision(&head, &path)?;
        target.materialize_artifact_state(&path, &checkout.state)?;
        assert_eq!(fs::read(path)?, bytes);
    }
    Ok(())
}

#[test]
#[ignore = "requires Node.js, built @eidos.space/graft-remote, and a loopback listener"]
fn encrypted_fresh_clone_restores_inline_pack_and_large_payload_with_four_mib_budget() -> Result<()>
{
    let fixture = Fixture::new()?;
    // A large-budget publisher also proves compatibility with packs created
    // before bounded pack construction, which exceed the reader's budget.
    let source = fixture.repo("source", 64 * 1024 * 1024)?;
    let mut expected = Vec::<(PathBuf, Vec<u8>)>::new();
    for index in 0..8 {
        expected.push((
            PathBuf::from(format!("small-{index}.txt")),
            vec![b'a' + index; 500 * 1024],
        ));
    }
    expected.push((PathBuf::from("large.bin"), vec![0; 7 * 1024 * 1024]));
    for (path, bytes) in &expected {
        fs::write(source.worktree().join(path), bytes)?;
        source.stage_artifact_path(source.worktree().join(path))?;
    }
    let head = source.commit_staged("inline pack and external payload")?.id;
    source.branch_create("fixture", Some(&head))?;
    source.push_branch("origin", "fixture", "fixture")?;
    let target = fixture.repo("target", 4 * 1024 * 1024)?;
    assert_eq!(
        target.fetch_for_clone("origin", "fixture")?.fetch.head,
        head
    );
    for (path, bytes) in &expected {
        let checkout =
            target.checkout_artifact_from_revision(&head, target.worktree().join(path))?;
        target.materialize_artifact_state(target.worktree().join(path), &checkout.state)?;
        assert_eq!(fs::read(target.worktree().join(path))?, *bytes);
    }
    assert!(fixture.capture()?.as_array().is_some());
    Ok(())
}

#[test]
#[ignore = "requires Node.js, built @eidos.space/graft-remote, and a loopback listener"]
fn encrypted_large_payload_streams_through_multipart_and_restores_exact_hash() -> Result<()> {
    use std::io::{Read, Write};
    let fixture = Fixture::with_fetch_fault(Some("multipart-file"))?;
    let source = fixture.repo("source", 64 * 1024)?;
    let path = source.worktree().join("large.bin");
    let mut file = fs::File::create(&path)?;
    let mut expected = blake3::Hasher::new();
    for index in 0..112 {
        let mut chunk = [0_u8; 64 * 1024];
        for (offset, byte) in chunk.iter_mut().enumerate() {
            *byte = ((index + offset) % 251) as u8;
        }
        file.write_all(&chunk)?;
        expected.update(&chunk);
    }
    file.sync_all()?;
    drop(file);
    source.stage_artifact_path(&path)?;
    let head = source.commit_staged("stream multipart payload")?.id;
    source.branch_create("large", Some(&head))?;
    source.push_branch("origin", "large", "large")?;
    let target = fixture.repo("target", 64 * 1024)?;
    assert_eq!(target.fetch_for_clone("origin", "large")?.fetch.head, head);
    let restored = target.worktree().join("large.bin");
    let checkout = target.checkout_artifact_from_revision(&head, &restored)?;
    target.materialize_artifact_state(&restored, &checkout.state)?;
    let mut file = fs::File::open(restored)?;
    assert_eq!(file.metadata()?.len(), 7 * 1024 * 1024);
    let mut actual = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        actual.update(&buffer[..read]);
    }
    assert_eq!(actual.finalize(), expected.finalize());
    // Cached payload validation in both prepare and finish stays streamed too.
    target.prepare_fetch("origin", "large")?.finish()?;
    let captures = fixture.capture()?;
    let requests = captures.as_array().ok_or("invalid captures")?;
    for operation in ["multipart-start", "multipart-part", "multipart-complete"] {
        assert!(
            requests.iter().any(|request| request["path"]
                .as_str()
                .is_some_and(|path| path.contains(operation))),
            "missing {operation}"
        );
    }
    Ok(())
}

#[test]
#[ignore = "requires Node.js, built @eidos.space/graft-remote, and a loopback listener"]
fn encrypted_push_and_fresh_clone_preserve_more_than_4096_commits() -> Result<()> {
    assert_history_roundtrip(4097, 4 * 1024 * 1024)
}

#[test]
#[ignore = "requires Node.js, built @eidos.space/graft-remote, and a loopback listener"]
fn encrypted_index_batches_and_pack_cache_respect_small_host_budget() -> Result<()> {
    assert_history_roundtrip(1000, 64 * 1024)
}

#[test]
#[ignore = "requires Node.js, built @eidos.space/graft-remote, and a loopback listener"]
fn encrypted_fresh_clone_reuses_bounded_packs_across_hash_ordered_files() -> Result<()> {
    use graft::repo::object::{BlobObject, FileBlob, FileContentKind, TreeEntry, TreeEntryMode};
    // Force the authenticated fallback, with a working set over eight times
    // its one-MiB budget. An all-packs-fit fixture cannot catch LRU thrashing.
    let fixture = Fixture::with_fetch_fault(Some("unavailable"))?;
    let source = fixture.repo("source", 1024 * 1024)?;
    let mut entries = Vec::new();
    for index in 0..2048 {
        let mut bytes = format!("unique synthetic file {index}").into_bytes();
        bytes.resize(4096, b'x');
        let oid = source
            .object_store()
            .write(&Object::Blob(BlobObject::File(FileBlob {
                kind: FileContentKind::TextFile,
                bytes,
            })))?;
        entries.push(TreeEntry {
            mode: TreeEntryMode::Regular,
            oid,
            path: format!("file-{index:05}.txt"),
        });
    }
    let tree = source
        .object_store()
        .write(&Object::Tree(TreeObject::new(entries.clone())?))?;
    let signature = Signature::new("Synthetic", "test@example.invalid", 1, "+0000");
    let head = source.object_store().write(&Object::Commit(CommitObject {
        tree,
        parents: vec![],
        author: signature.clone(),
        committer: signature,
        repo_format_version: 1,
        tables: vec![],
        path_changes: None,
        message: "bounded pack reuse".into(),
    }))?;
    source.branch_create("files", Some(head.as_str()))?;
    source.push_branch("origin", "files", "files")?;
    let before = fixture.capture()?.as_array().unwrap().len();
    let target = fixture.repo("target", 1024 * 1024)?;
    let fetched = target.fetch_for_clone("origin", "files")?.fetch;
    assert_eq!(fetched.head, head.as_str());
    assert_eq!(fetched.commits, 1);
    for entry in entries {
        target.object_store().read(&entry.oid)?;
    }
    let after = fixture.capture()?.as_array().unwrap().len();
    assert!(
        after - before < 120,
        "bounded pack working set caused {} requests",
        after - before
    );
    Ok(())
}

#[test]
#[ignore = "requires Node.js, built @eidos.space/graft-remote, and a loopback listener"]
fn encrypted_incremental_receipt_validates_new_objects_and_clears_on_lock_and_reopen() -> Result<()>
{
    let fixture = Fixture::new()?;
    let source = fixture.repo("source", 4 * 1024 * 1024)?;
    let old_path = source.worktree().join("old.txt");
    fs::write(&old_path, b"old verified data")?;
    source.stage_artifact_path(&old_path)?;
    let first = source.commit_staged("first")?.id;
    source.branch_create("first", Some(&first))?;
    source.push_branch("origin", "first", "main")?;
    let (target, secret, _) = fixture.repo_session("target", 4 * 1024 * 1024)?;
    target
        .fetch("origin", "main")
        .expect("initial complete fetch");
    let Object::Commit(first_commit) = source.read_object(&first)? else {
        panic!("commit")
    };
    let Object::Tree(first_tree) = source.object_store().read(&first_commit.tree)? else {
        panic!("tree")
    };
    let old_id = first_tree.entries[0].oid.clone();
    fs::remove_file(target.object_store().path_for(&old_id))?;
    let new_path = source.worktree().join("new.txt");
    fs::write(&new_path, b"new authoritative data")?;
    source.stage_artifact_path(&new_path)?;
    let second = source.commit_staged("second")?.id;
    source.branch_create("second", Some(&second))?;
    source.push_branch("origin", "second", "main")?;
    let Object::Commit(second_commit) = source.read_object(&second)? else {
        panic!("commit")
    };
    let Object::Tree(second_tree) = source.object_store().read(&second_commit.tree)? else {
        panic!("tree")
    };
    let new_id = second_tree
        .entries
        .iter()
        .find(|entry| entry.path == "new.txt")
        .unwrap()
        .oid
        .clone();
    let new_cache = target.object_store().path_for(&new_id);
    fs::create_dir_all(new_cache.parent().unwrap())?;
    fs::write(&new_cache, b"forged new cache bytes")?;
    assert!(target.fetch("origin", "main").is_err());
    assert_eq!(
        target.remote_tracking_ref("origin", "main")?.as_deref(),
        Some(first.as_str())
    );
    fs::remove_file(&new_cache)?;
    target
        .fetch("origin", "main")
        .expect("fetch missing new object");
    target
        .object_store()
        .read(&new_id)
        .expect("new object is restored");
    // Prior closure is trusted until actually consumed; ordinary incremental
    // fetch is not an unsolicited complete cache-repair scan.
    assert!(target.object_store().read(&old_id).is_err());
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(secret.lock());
    secret.unlock_with_root(
        &fixture.config,
        RootKey::import(fixture.identity, zeroize::Zeroizing::new([42; 32])),
    )?;
    target
        .fetch("origin", "main")
        .expect("post-lock cold closure");
    target
        .object_store()
        .read(&old_id)
        .expect("post-lock repairs old blob");
    fs::remove_file(target.object_store().path_for(&old_id))?;
    let (_, _, fresh_credentials) = fixture.repo_session("reopen", 4 * 1024 * 1024)?;
    let reopened = Repository::open(target.worktree())?.with_remote_credentials(fresh_credentials);
    reopened
        .fetch("origin", "main")
        .expect("reopened cold closure");
    reopened
        .object_store()
        .read(&old_id)
        .expect("reopened repairs old blob");
    // Missing old root metadata cancels the receipt and obtains full closure.
    fs::remove_file(reopened.object_store().path_for(&second_commit.tree))?;
    fs::remove_file(reopened.object_store().path_for(&old_id))?;
    reopened
        .fetch("origin", "main")
        .expect("missing prior root metadata falls back");
    reopened
        .object_store()
        .read(&old_id)
        .expect("missing root repairs old blob");
    // Replacing the canonical store at the same path must not inherit a receipt
    // from its old inode, even when the advertised commit/tree roots match.
    let replacement = fixture.repo("replacement", 4 * 1024 * 1024)?;
    for id in [second.parse()?, second_commit.tree] {
        replacement
            .object_store()
            .write_raw_validated(&id, &source.object_store().read_raw(&id)?.unwrap())?;
    }
    replacement.set_remote_tracking_ref("origin", "main", &second)?;
    fs::rename(
        reopened.worktree().join(".graft"),
        fixture.directory.path().join("old-graft"),
    )?;
    fs::rename(
        replacement.worktree().join(".graft"),
        reopened.worktree().join(".graft"),
    )?;
    reopened
        .fetch("origin", "main")
        .expect("new store inode obtains complete closure");
    reopened
        .object_store()
        .read(&old_id)
        .expect("new store repairs old blob");
    Ok(())
}

#[test]
#[ignore = "requires Node.js, built @eidos.space/graft-remote, and a loopback listener"]
fn encrypted_prepared_fetch_advertises_only_locally_verified_commits() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.repo("source", 4 * 1024 * 1024)?;
    let path = source.worktree().join("private.txt");
    fs::write(&path, b"verified incremental fetch")?;
    source.stage_artifact_path(path)?;
    let head = source.commit_staged("first")?.id;
    source.branch_create("first", Some(&head))?;
    source.push_branch("origin", "first", "first")?;
    let target = fixture.repo("target", 4 * 1024 * 1024)?;
    // A tracking pointer alone is not evidence of a locally verified commit.
    target.set_remote_tracking_ref("origin", "first", &head)?;
    assert!(target.object_store().read_raw(&head.parse()?)?.is_none());
    assert_eq!(
        target.prepare_fetch("origin", "first")?.finish()?.head,
        head
    );
    target.prepare_fetch("origin", "first")?.finish()?;
    let captures = fixture.capture()?;
    let requests = captures.as_array().ok_or("invalid captures")?;
    let controls = requests
        .iter()
        .filter(|request| {
            request["path"]
                .as_str()
                .is_some_and(|path| path.contains("/e2ee/fetch-bundle/"))
        })
        .map(|request| serde_json::from_str::<serde_json::Value>(request["body"].as_str().unwrap()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(controls.len(), 2);
    assert!(controls[0]["have"].is_null());
    let root = RootKey::import(fixture.identity, zeroize::Zeroizing::new([42; 32]));
    let route = root.route_commit(blake3::Hash::from_hex(&head)?.as_bytes())?;
    let expected: String = route.iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(controls[1]["have"], expected);
    Ok(())
}

#[test]
#[ignore = "requires Node.js, built @eidos.space/graft-remote, and a loopback listener"]
fn encrypted_fetch_bundle_rejects_untrusted_ref_frames_and_hints_without_fallback() -> Result<()> {
    for fault in [
        "ref-key",
        "ref-ciphertext",
        "framing",
        "hint-mac",
        "hint-topology",
    ] {
        let fixture = Fixture::with_fetch_fault(Some(fault))?;
        let source = fixture.repo("source", 4 * 1024 * 1024)?;
        let path = source.worktree().join("private-name.txt");
        fs::write(&path, b"private contents")?;
        source.stage_artifact_path(path)?;
        let head = source.commit_staged("private message")?.id;
        source.branch_create("private-branch", Some(&head))?;
        source.push_branch("origin", "private-branch", "private-branch")?;
        let target = fixture.repo("target", 4 * 1024 * 1024)?;
        let result = target.fetch_for_clone("origin", "private-branch");
        assert!(result.is_err(), "{fault}");
        if fault == "hint-topology" {
            assert!(
                format!("{:?}", result.err().ok_or("expected mismatch")?).contains("HintMismatch")
            );
        }
        assert!(
            target
                .remote_tracking_ref("origin", "private-branch")?
                .is_none()
        );
        assert!(target.object_store().read_raw(&head.parse()?)?.is_none());
        let captures = fixture.capture()?;
        let requests = captures.as_array().ok_or("invalid captures")?;
        let index = requests
            .iter()
            .position(|request| {
                request["path"]
                    .as_str()
                    .is_some_and(|path| path.contains("/e2ee/fetch-bundle/"))
            })
            .ok_or("aggregate fetch was not attempted")?;
        assert_eq!(
            index + 1,
            requests.len(),
            "unexpected fallback after {fault}"
        );
    }
    Ok(())
}

#[test]
#[ignore = "requires Node.js, built @eidos.space/graft-remote, and a loopback listener"]
fn encrypted_fetch_bundle_unavailable_uses_only_verified_encrypted_fallback() -> Result<()> {
    let fixture = Fixture::with_fetch_fault(Some("unavailable"))?;
    let source = fixture.repo("source", 4 * 1024 * 1024)?;
    let path = source.worktree().join("private-name.txt");
    fs::write(&path, b"private contents")?;
    source.stage_artifact_path(path)?;
    let head = source.commit_staged("private message")?.id;
    source.branch_create("private-branch", Some(&head))?;
    source.push_branch("origin", "private-branch", "private-branch")?;
    let target = fixture.repo("target", 4 * 1024 * 1024)?;
    assert_eq!(
        target
            .fetch_for_clone("origin", "private-branch")?
            .fetch
            .head,
        head
    );
    let capture = fixture.capture()?;
    let capture = serde_json::to_string(&capture)?;
    assert!(capture.contains("/e2ee/fetch-bundle/"));
    for plaintext in [
        "private-branch",
        "private-name.txt",
        "private contents",
        "private message",
    ] {
        assert!(
            !capture.contains(plaintext),
            "plaintext packet leak: {plaintext}"
        );
    }
    Ok(())
}

#[test]
#[ignore = "requires Node.js, built @eidos.space/graft-remote, and a loopback listener"]
fn encrypted_receive_bundle_benign_ciphertext_collision_keeps_exact_ref_cas() -> Result<()> {
    let fixture = Fixture::with_fetch_fault(Some("receive-collision"))?;
    let source = fixture.repo("source", 4 * 1024 * 1024)?;
    let path = source.worktree().join("private.txt");
    fs::write(
        &path,
        b"same canonical content with independently randomized envelopes",
    )?;
    source.stage_artifact_path(path)?;
    let head = source.commit_staged("first")?.id;
    source.branch_create("first", Some(&head))?;
    source.push_branch("origin", "first", "first")?;
    // Omitted immutable listing forces re-encryption of the same canonical pack.
    source.branch_create("second", Some(&head))?;
    source.push_branch("origin", "second", "second")?;
    let requests = fixture.capture()?;
    let requests = requests.as_array().ok_or("invalid captures")?;
    let collision = requests
        .iter()
        .position(|request| {
            request["path"]
                .as_str()
                .is_some_and(|path| path.contains("/e2ee/receive-bundle/"))
                && request["responseStatus"] == 412
        })
        .ok_or("expected genuine randomized-ciphertext collision")?;
    let fallback = requests[collision + 1..]
        .iter()
        .find(|request| {
            request["path"]
                .as_str()
                .is_some_and(|path| path.contains("/cas/e2ee/refs/"))
        })
        .ok_or("expected exact ref primitive fallback")?;
    assert_eq!(requests[collision]["expectedHex"], fallback["expectedHex"]);
    assert_eq!(
        receive_candidate_hex(&requests[collision])?,
        fallback["body"].as_str().ok_or("missing fallback body")?
    );
    let target = fixture.repo("target", 4 * 1024 * 1024)?;
    assert_eq!(target.fetch_for_clone("origin", "second")?.fetch.head, head);
    Ok(())
}

#[test]
#[ignore = "requires Node.js, built @eidos.space/graft-remote, and a loopback listener"]
fn encrypted_receive_bundle_lost_response_retains_and_reconciles_journal() -> Result<()> {
    let fixture = Fixture::with_fetch_fault(Some("receive-loss"))?;
    let source = fixture.repo("source", 4 * 1024 * 1024)?;
    let path = source.worktree().join("private.txt");
    fs::write(&path, b"retained receive candidate")?;
    source.stage_artifact_path(path)?;
    let head = source.commit_staged("first")?.id;
    source.branch_create("first", Some(&head))?;
    let outcome = source.push_branch("origin", "first", "first");
    assert!(format!("{outcome:?}").contains("EncryptedOutcomeUnknown"));
    let retry = source.push_branch("origin", "first", "first")?;
    assert_eq!(retry.head, head);
    assert_eq!(retry.commits, 0);
    let requests = fixture.capture()?;
    let requests = requests.as_array().ok_or("invalid captures")?;
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["path"]
                .as_str()
                .is_some_and(|path| path.contains("/e2ee/receive-bundle/")))
            .count(),
        1
    );
    assert!(!requests.iter().any(|request| {
        request["path"]
            .as_str()
            .is_some_and(|path| path.contains("/cas/e2ee/refs/"))
    }));
    use graft::e2ee::RefMutationJournalProvider;
    let binding = SessionBinding::new(
        "pack-regression.test".into(),
        "synthetic-account".into(),
        fixture.identity,
    )?;
    let provider = graft::e2ee::FileRefMutationJournalProvider::open(
        fixture.directory.path().join("journals-source"),
        &binding,
    )?;
    let received = requests
        .iter()
        .find(|request| {
            request["path"]
                .as_str()
                .is_some_and(|path| path.contains("/e2ee/receive-bundle/"))
        })
        .ok_or("missing receive")?;
    let key = received["path"]
        .as_str()
        .ok_or("missing path")?
        .split("/e2ee/receive-bundle/")
        .nth(1)
        .ok_or("missing opaque ref")?;
    assert!(provider.journal(&binding, key)?.load(&binding)?.is_none());
    let target = fixture.repo("target", 4 * 1024 * 1024)?;
    assert_eq!(target.fetch_for_clone("origin", "first")?.fetch.head, head);
    Ok(())
}

#[test]
#[ignore = "requires Node.js, built @eidos.space/graft-remote, and a loopback listener"]
fn encrypted_retry_keeps_original_candidate_after_authenticated_competing_ref() -> Result<()> {
    use graft::e2ee::RefMutationJournalProvider;
    let fixture = Fixture::with_fetch_fault(Some("receive-loss"))?;
    let source = fixture.repo("source", 4 * 1024 * 1024)?;
    let path = source.worktree().join("private.txt");
    fs::write(&path, b"retained receive candidate")?;
    source.stage_artifact_path(path)?;
    let head = source.commit_staged("first")?.id;
    source.branch_create("first", Some(&head))?;
    let outcome = source.push_branch("origin", "first", "first");
    assert!(format!("{outcome:?}").contains("EncryptedOutcomeUnknown"));
    let competitor = fixture.repo("competitor", 4 * 1024 * 1024)?;
    competitor.fetch_for_clone("origin", "first")?;
    let Object::Commit(mut commit) = competitor.read_object(&head)? else {
        return Err("expected commit".into());
    };
    commit.parents = vec![head.parse()?];
    commit.message = "competing successor".into();
    let next = competitor.object_store().write(&Object::Commit(commit))?;
    competitor.branch_create("successor", Some(next.as_str()))?;
    competitor.push_branch("origin", "successor", "first")?;
    let retry = source.push_branch_with_force("origin", "first", "first", true);
    assert!(
        matches!(retry, Err(graft::repo::RepoErr::RemoteRefChanged { .. })),
        "{retry:?}"
    );
    let requests = fixture.capture()?;
    let requests = requests.as_array().ok_or("invalid captures")?;
    let original = requests
        .iter()
        .find(|request| request["droppedReceiveResponse"] == true)
        .ok_or("missing original receive")?;
    let retry = requests
        .iter()
        .rev()
        .find(|request| {
            request["path"]
                .as_str()
                .is_some_and(|path| path.contains("/cas/e2ee/refs/"))
        })
        .ok_or("missing exact CAS retry")?;
    assert_eq!(retry["responseStatus"], 409);
    assert_eq!(retry["expectedHex"], original["expectedHex"]);
    assert_eq!(retry["body"], receive_candidate_hex(original)?);
    let binding = SessionBinding::new(
        "pack-regression.test".into(),
        "synthetic-account".into(),
        fixture.identity,
    )?;
    let provider = graft::e2ee::FileRefMutationJournalProvider::open(
        fixture.directory.path().join("journals-source"),
        &binding,
    )?;
    let key = original["path"]
        .as_str()
        .unwrap()
        .split("/e2ee/receive-bundle/")
        .nth(1)
        .unwrap();
    assert!(provider.journal(&binding, key)?.load(&binding)?.is_none());
    let reader = fixture.repo("reader", 4 * 1024 * 1024)?;
    assert_eq!(
        reader.fetch_for_clone("origin", "first")?.fetch.head,
        next.as_str()
    );
    Ok(())
}

fn receive_candidate_hex(request: &serde_json::Value) -> Result<String> {
    let text = request["body"].as_str().ok_or("missing bundle body")?;
    let bytes = (0..text.len())
        .step_by(2)
        .map(|offset| u8::from_str_radix(&text[offset..offset + 2], 16))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut cursor = 14;
    for field in 0..3 {
        let length = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into()?) as usize;
        cursor += 4;
        if field == 2 {
            return Ok(bytes[cursor..cursor + length]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect());
        }
        cursor += length;
    }
    Err("missing candidate".into())
}

fn assert_history_roundtrip(count: usize, budget: usize) -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.repo("history-source", budget)?;
    let store = source.object_store();
    let tree = store.write(&Object::Tree(TreeObject::new(vec![])?))?;
    let mut parent = None;
    let mut commits = Vec::new();
    for index in 0..count {
        let signature = Signature::new("Test", "test@example.invalid", index as u64, "+0000");
        let commit = Object::Commit(CommitObject {
            tree: tree.clone(),
            parents: parent.into_iter().collect(),
            author: signature.clone(),
            committer: signature,
            repo_format_version: 1,
            tables: vec![],
            path_changes: None,
            message: format!("Commit {index}"),
        });
        let id = store.write(&commit)?;
        commits.push((id.clone(), commit.canonical_bytes()));
        parent = Some(id);
    }
    let head = parent.ok_or("missing head")?;
    source.branch_create("history", Some(head.as_str()))?;
    assert_eq!(
        source.push_branch("origin", "history", "history")?.commits,
        count
    );
    let target = fixture.repo("history-target", budget)?;
    assert_eq!(
        target.fetch_for_clone("origin", "history")?.fetch.head,
        head.as_str()
    );
    let captures = fixture.capture()?;
    assert!(
        captures
            .as_array()
            .ok_or("invalid captures")?
            .iter()
            .any(|request| {
                request["path"]
                    .as_str()
                    .is_some_and(|path| path.contains("/e2ee/fetch-bundle/"))
                    && request["responseStatus"] == 200
            })
    );
    for (id, expected) in commits {
        assert_eq!(target.read_object(id.as_str())?.canonical_bytes(), expected);
    }
    Ok(())
}
