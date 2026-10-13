//! Synthetic paired load evidence; run the Python driver for sampled process RSS/CPU.
#[cfg(unix)]
mod benchmark {
    use graft::{
        e2ee::*,
        remote::{EncryptedTransferBudget, RemoteConfig, RemoteCredentials},
        repo::{Repository, object::*},
    };
    use serde_json::{Value, json};
    use std::{
        fs,
        io::{BufRead, BufReader, Read, Write},
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::{Child, ChildStdin, ChildStdout, Command, Stdio},
        time::Instant,
    };
    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
    const UUID: &str = "00010203-0405-4607-8809-0a0b0c0d0e0f";
    const PASSWORD: &[u8] = b"synthetic load benchmark password";
    fn identity() -> RepositoryId {
        RepositoryId::from_bytes([0, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
            .unwrap()
    }
    fn root_key() -> RootKey {
        RootKey::import(identity(), zeroize::Zeroizing::new([42; 32]))
    }
    fn private(root: &Path, name: &str) -> Result<PathBuf> {
        let path = root.join(name);
        fs::create_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(path)
    }
    fn event(event: &str, operation: &str) {
        let handshake = std::env::var("GRAFT_BENCHMARK_RESOURCE_HANDSHAKE").as_deref() == Ok("1");
        println!(
            "{}",
            json!({"event":event,"operation":operation,"pid":std::process::id(),"resource_handshake":handshake})
        );
        if handshake {
            std::io::stdout().flush().expect("flush resource event");
            let mut acknowledgement = String::new();
            std::io::stdin()
                .read_line(&mut acknowledgement)
                .expect("resource acknowledgement");
            assert_eq!(
                acknowledgement, "continue\n",
                "invalid resource acknowledgement"
            );
        }
    }
    struct Service {
        child: Child,
        input: ChildStdin,
        output: BufReader<ChildStdout>,
    }
    impl Drop for Service {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
    impl Service {
        fn read(&mut self) -> Result<Value> {
            let mut line = String::new();
            self.output.read_line(&mut line)?;
            Ok(serde_json::from_str(&line)?)
        }
        fn command(&mut self, command: &str) -> Result<Value> {
            writeln!(self.input, "{command}")?;
            self.input.flush()?;
            self.read()
        }
        fn measure<T>(
            &mut self,
            name: &str,
            work: impl FnOnce() -> Result<T>,
        ) -> Result<(Value, T)> {
            let before = self.command("snapshot")?;
            event("begin", name);
            let start = Instant::now();
            let result = work()?;
            let milliseconds = start.elapsed().as_secs_f64() * 1000.;
            event("end", name);
            let mut metrics = self.command("snapshot")?;
            metrics["milliseconds"] = json!(milliseconds);
            metrics["operation"] = json!(name);
            metrics["server_cpu_microseconds"] = json!({"user":metrics["server_cpu"]["user"].as_u64().unwrap()-before["server_cpu"]["user"].as_u64().unwrap(),"system":metrics["server_cpu"]["system"].as_u64().unwrap()-before["server_cpu"]["system"].as_u64().unwrap()});
            Ok((metrics, result))
        }
    }
    fn repository(
        root: &Path,
        name: &str,
        remote: &RemoteConfig,
        config: &EncryptedConfig,
        maximum: usize,
    ) -> Result<Repository> {
        let credentials = RemoteCredentials::explicit();
        credentials.set_http_bearer_token("origin", "benchmark-token".into())?;
        if matches!(remote, RemoteConfig::EncryptedHttp { .. }) {
            let binding = SessionBinding::new(
                "load-benchmark.test".into(),
                "synthetic-account".into(),
                identity(),
            )?;
            let checkpoint = FileCheckpointStore::create(
                private(root, &format!("{name}-checkpoint"))?,
                &binding,
            )?;
            let session =
                SecretSession::with_checkpoint_store(binding, std::sync::Arc::new(checkpoint))?;
            session.unlock_with_root(config, root_key())?;
            let journals = FileRefMutationJournalProvider::create(
                private(root, &format!("{name}-refs"))?,
                session.binding(),
            )?;
            let uploads = FileMultipartJournalProvider::create(
                private(root, &format!("{name}-uploads"))?,
                session.binding(),
            )?;
            credentials.set_encrypted_session_with_recovery(
                "origin",
                remote,
                session,
                EncryptedTransferBudget {
                    private_directory: private(root, &format!("{name}-staging"))?,
                    max_buffered_plaintext: maximum,
                    max_ciphertext_spool: 256 * 1024 * 1024,
                },
                std::sync::Arc::new(journals),
                std::sync::Arc::new(uploads),
            )?;
        }
        let repo = Repository::init(root.join(name))?.with_remote_credentials(credentials);
        repo.remote_add("origin", remote.clone())?;
        Ok(repo)
    }
    fn commit(
        repo: &Repository,
        tree: ObjectId,
        parent: Option<ObjectId>,
        message: &str,
        stamp: u64,
    ) -> Result<ObjectId> {
        let signature = Signature::new("Synthetic", "benchmark@example.invalid", stamp, "+0000");
        Ok(repo.object_store().write(&Object::Commit(CommitObject {
            tree,
            parents: parent.into_iter().collect(),
            author: signature.clone(),
            committer: signature,
            repo_format_version: 1,
            tables: vec![],
            path_changes: None,
            message: message.into(),
        }))?)
    }
    fn file_tree(repo: &Repository, count: usize) -> Result<(ObjectId, Vec<ObjectId>)> {
        let mut entries = Vec::with_capacity(count);
        let mut objects = Vec::with_capacity(count + 1);
        for index in 0..count {
            let oid = repo
                .object_store()
                .write(&Object::Blob(BlobObject::File(FileBlob {
                    kind: FileContentKind::TextFile,
                    bytes: format!("synthetic object {index}\n").into_bytes(),
                })))?;
            entries.push(TreeEntry {
                mode: TreeEntryMode::Regular,
                oid: oid.clone(),
                path: format!("file-{index:06}.txt"),
            });
            objects.push(oid);
        }
        let tree = repo
            .object_store()
            .write(&Object::Tree(TreeObject::new(entries)?))?;
        objects.push(tree.clone());
        Ok((tree, objects))
    }
    fn attachment_tree(
        repo: &Repository,
        bytes: usize,
    ) -> Result<(ObjectId, Vec<ObjectId>, ObjectId)> {
        let mut file = tempfile::NamedTempFile::new_in(repo.worktree())?;
        let mut hash = blake3::Hasher::new();
        let buffer: Vec<u8> = (0..64 * 1024).map(|index| (index % 251) as u8).collect();
        let mut remaining = bytes;
        while remaining > 0 {
            let count = remaining.min(buffer.len());
            file.write_all(&buffer[..count])?;
            hash.update(&buffer[..count]);
            remaining -= count;
        }
        let payload: ObjectId = hash.finalize().to_hex().as_str().parse()?;
        let target = repo
            .file_store_dir()
            .join(&payload.as_str()[..2])
            .join(&payload.as_str()[2..]);
        fs::create_dir_all(target.parent().unwrap())?;
        file.persist(target)?;
        let pointer = repo
            .object_store()
            .write(&Object::Blob(BlobObject::LargeFilePointer(
                LargeFilePointerBlob {
                    kind: FileContentKind::BinaryFile,
                    content_hash: payload.clone(),
                    size: bytes as u64,
                },
            )))?;
        let tree = repo
            .object_store()
            .write(&Object::Tree(TreeObject::new(vec![TreeEntry {
                mode: TreeEntryMode::Regular,
                oid: pointer.clone(),
                path: "large.bin".into(),
            }])?))?;
        Ok((tree.clone(), vec![pointer, tree], payload))
    }
    fn verify(
        repo: &Repository,
        objects: &[ObjectId],
        payload: Option<(&ObjectId, usize)>,
    ) -> Result<()> {
        for id in objects {
            let bytes = fs::read(repo.object_store().path_for(id))?;
            assert_eq!(ObjectId::for_bytes(&bytes), *id);
            Object::decode(&bytes)?;
        }
        if let Some((id, size)) = payload {
            let mut file = fs::File::open(
                repo.file_store_dir()
                    .join(&id.as_str()[..2])
                    .join(&id.as_str()[2..]),
            )?;
            assert_eq!(file.metadata()?.len(), size as u64);
            let mut hash = blake3::Hasher::new();
            let mut buffer = [0; 64 * 1024];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
            }
            assert_eq!(hash.finalize().to_hex().as_str(), id.as_str());
        }
        Ok(())
    }
    pub fn run() -> Result<()> {
        let args: Vec<_> = std::env::args().collect();
        if args.len() != 5 {
            return Err("usage: e2ee_load_benchmark REPORT.json plain|encrypted objects|history|attachment|same-ref|different-ref SCALE".into());
        }
        let profile = &args[2];
        if !["plain", "encrypted"].contains(&profile.as_str()) {
            return Err("invalid profile".into());
        }
        let scenario = &args[3];
        let scale: usize = args[4].parse()?;
        if scale == 0 {
            return Err("scale must be positive".into());
        }
        let directory = tempfile::tempdir()?;
        event("begin", "kdf_create");
        let kdf = Instant::now();
        let config = EncryptedConfig::create(&root_key(), PASSWORD)?;
        let kdf_create = kdf.elapsed().as_secs_f64() * 1000.;
        event("end", "kdf_create");
        event("begin", "kdf_unlock");
        let kdf = Instant::now();
        config.unlock(PASSWORD, Some(identity()))?;
        let kdf_unlock = kdf.elapsed().as_secs_f64() * 1000.;
        event("end", "kdf_unlock");
        let fixture = directory.path().join("fixture.json");
        fs::write(
            &fixture,
            serde_json::to_vec(
                &json!({"config":config.as_bytes(),"resourceMetrics":true,"encryption":{"profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":UUID}}),
            )?,
        )?;
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut child = Command::new("node")
            .arg(workspace.join("tests/e2ee/benchmark-service.mjs"))
            .arg(fixture)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let mut service = Service {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
        };
        let origin = service.read()?["origin"]
            .as_str()
            .ok_or("missing origin")?
            .to_string();
        let remote = if profile == "plain" {
            RemoteConfig::Http {
                url: format!("{origin}/plain"),
                token_env: None,
            }
        } else {
            RemoteConfig::EncryptedHttp {
                url: format!("{origin}/encrypted"),
                uuid: UUID.into(),
            }
        };
        let maximum = if scenario == "objects" {
            16 * 1024 * 1024
        } else {
            4 * 1024 * 1024
        };
        let source = repository(directory.path(), "source", &remote, &config, maximum)?;
        let mut samples = Vec::new();
        let evidence;
        if scenario == "same-ref" || scenario == "different-ref" {
            let (tree, _) = file_tree(&source, 1)?;
            let base = commit(&source, tree, None, "base", 0)?;
            source.branch_create("base", Some(base.as_str()))?;
            source.push_branch("origin", "base", "main")?;
            let left = repository(directory.path(), "left", &remote, &config, maximum)?;
            let right = repository(directory.path(), "right", &remote, &config, maximum)?;
            left.fetch_for_clone("origin", "main")?;
            right.fetch_for_clone("origin", "main")?;
            let (lt, _) = file_tree(&left, 2)?;
            let (rt, _) = file_tree(&right, 3)?;
            let a = commit(&left, lt, Some(base.clone()), "left", 1)?;
            let b = commit(&right, rt, Some(base), "right", 2)?;
            left.branch_create("next", Some(a.as_str()))?;
            right.branch_create("next", Some(b.as_str()))?;
            let right_branch = if scenario == "same-ref" {
                "main"
            } else {
                "independent"
            };
            let lo = left.observe_remote_branch_head("origin", "main")?;
            let ro = right.observe_remote_branch_head("origin", right_branch)?;
            let (metric, (l, r)) = service.measure("concurrent_push", || {
                Ok(std::thread::scope(|scope| {
                    let l = scope.spawn(|| {
                        left.push_branch_with_observed_head_and_snapshots(
                            "origin",
                            "next",
                            "main",
                            false,
                            lo,
                            graft::PreparedSnapshotPush::default(),
                        )
                    });
                    let r = scope.spawn(|| {
                        right.push_branch_with_observed_head_and_snapshots(
                            "origin",
                            "next",
                            right_branch,
                            false,
                            ro,
                            graft::PreparedSnapshotPush::default(),
                        )
                    });
                    (l.join().unwrap(), r.join().unwrap())
                }))
            })?;
            samples.push(metric);
            let successes = usize::from(l.is_ok()) + usize::from(r.is_ok());
            assert_eq!(successes, if scenario == "same-ref" { 1 } else { 2 });
            if scenario == "same-ref" {
                let failure = if l.is_err() { &l } else { &r };
                assert!(matches!(
                    failure,
                    Err(graft::repo::RepoErr::RemoteRefChanged { .. })
                ));
            }
            let reader = repository(directory.path(), "reader", &remote, &config, maximum)?;
            let selected = reader.fetch_for_clone("origin", "main")?.fetch.head;
            assert_eq!(
                selected,
                if l.is_ok() {
                    l.as_ref().unwrap().head.clone()
                } else {
                    r.as_ref().unwrap().head.clone()
                }
            );
            let mut heads = vec![selected];
            if scenario == "different-ref" {
                let selected = reader.fetch_for_clone("origin", right_branch)?.fetch.head;
                assert_eq!(selected, r.as_ref().unwrap().head);
                heads.push(selected);
            }
            let inventory = service.command("inventory")?;
            let entries = inventory[profile].as_array().unwrap();
            let mut pending = heads
                .into_iter()
                .map(|head| -> Result<String> {
                    if profile == "plain" {
                        return Ok(head);
                    }
                    Ok(root_key()
                        .route_commit(blake3::Hash::from_hex(&head)?.as_bytes())?
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect())
                })
                .collect::<Result<Vec<_>>>()?;
            let mut seen = std::collections::BTreeSet::new();
            let mut reachable_keys = std::collections::BTreeSet::new();
            while let Some(token) = pending.pop() {
                if !seen.insert(token.clone()) {
                    continue;
                }
                let mut found = false;
                for entry in entries {
                    let (metadata, identity) = if profile == "encrypted" {
                        (&entry["hint"], "token")
                    } else {
                        (&entry["index"], "id")
                    };
                    if let Some(commit) = metadata["commits"]
                        .as_array()
                        .and_then(|commits| commits.iter().find(|commit| commit[identity] == token))
                    {
                        found = true;
                        reachable_keys.insert(entry["key"].as_str().unwrap().to_string());
                        reachable_keys.insert(metadata["pack"].as_str().unwrap().to_string());
                        if profile == "encrypted" {
                            reachable_keys.insert(metadata["index"].as_str().unwrap().to_string());
                        }
                        pending.extend(
                            commit["parents"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|parent| parent.as_str().unwrap().to_string()),
                        );
                    }
                }
                assert!(
                    found,
                    "authenticated branch ancestry missing from inventory"
                );
            }
            // This inline-only fixture has no detached payloads or snapshot
            // segments. Count every stored immutable pack/index/hint outside
            // the complete ancestry of the freshly authenticated branch heads.
            let orphan_keys = entries
                .iter()
                .filter(|entry| {
                    let key = entry["key"].as_str().unwrap();
                    let immutable = if profile == "encrypted" {
                        key.starts_with("e2ee/packs/")
                            || key.starts_with("e2ee/indexes/")
                            || key.starts_with("e2ee/hints/")
                    } else {
                        key.starts_with("objects/pack/")
                    };
                    immutable && !reachable_keys.contains(key)
                })
                .map(|entry| entry["key"].as_str().unwrap().to_owned())
                .collect::<std::collections::BTreeSet<_>>();
            let orphan_bytes: u64 = entries
                .iter()
                .filter(|entry| orphan_keys.contains(entry["key"].as_str().unwrap()))
                .map(|entry| entry["bytes"].as_u64().unwrap())
                .sum();
            evidence = json!({"confirmed_publications":successes,"conclusive_conflicts":2-successes,"orphan_pack_bytes":orphan_bytes,"orphan_objects":orphan_keys.len(),"orphan_scope":"all immutable inventory outside authenticated branch ancestry in this inline-only fixture","refs_verified":true});
        } else {
            let (tree, mut objects, payload) = match scenario.as_str() {
                "objects" => {
                    if !(3..=100_000).contains(&scale) {
                        return Err("objects scale must be 3..100000".into());
                    }
                    let (tree, objects) = file_tree(&source, scale - 2)?;
                    (tree, objects, None)
                }
                "history" => {
                    let (tree, objects) = file_tree(&source, 0)?;
                    (tree, objects, None)
                }
                "attachment" => {
                    let (tree, objects, payload) = attachment_tree(&source, scale)?;
                    (tree, objects, Some(payload))
                }
                _ => return Err("invalid scenario".into()),
            };
            let first = commit(&source, tree.clone(), None, "base", 0)?;
            objects.push(first.clone());
            let mut head = first.clone();
            let resident = repository(directory.path(), "resident", &remote, &config, maximum)?;
            if scenario == "history" {
                source.branch_create("base", Some(first.as_str()))?;
                source.push_branch("origin", "base", "main")?;
                resident.fetch_for_clone("origin", "main")?;
                for index in 1..scale {
                    head = commit(
                        &source,
                        tree.clone(),
                        Some(head),
                        &format!("offline {index}"),
                        index as u64,
                    )?;
                    objects.push(head.clone());
                }
            }
            source.branch_create("next", Some(head.as_str()))?;
            samples.push(
                service
                    .measure(
                        if scenario == "history" {
                            "long_offline_push"
                        } else {
                            "initial_push"
                        },
                        || {
                            source.push_branch("origin", "next", "main")?;
                            Ok(())
                        },
                    )?
                    .0,
            );
            if scenario == "history" {
                samples.push(
                    service
                        .measure("long_offline_fetch", || {
                            assert_eq!(resident.fetch("origin", "main")?.head, head.as_str());
                            Ok(())
                        })?
                        .0,
                );
            }
            let reader = repository(directory.path(), "reader", &remote, &config, maximum)?;
            samples.push(
                service
                    .measure("cold_clone_sync", || {
                        assert_eq!(
                            reader.fetch_for_clone("origin", "main")?.fetch.head,
                            head.as_str()
                        );
                        Ok(())
                    })?
                    .0,
            );
            verify(&reader, &objects, payload.as_ref().map(|id| (id, scale)))?;
            for round in 1..=3 {
                let next = commit(
                    &source,
                    tree.clone(),
                    Some(head.clone()),
                    &format!("small {round}"),
                    (scale + round) as u64,
                )?;
                let branch = format!("update-{round}");
                source.branch_create(&branch, Some(next.as_str()))?;
                let mut push = service
                    .measure("warm_small_push", || {
                        source.push_branch("origin", &branch, "main")?;
                        Ok(())
                    })?
                    .0;
                push["round"] = json!(round);
                samples.push(push);
                let mut fetch = service
                    .measure("warm_small_fetch", || {
                        assert_eq!(reader.fetch("origin", "main")?.head, next.as_str());
                        Ok(())
                    })?
                    .0;
                fetch["round"] = json!(round);
                samples.push(fetch);
                head = next;
            }
            evidence = json!({"canonical_objects_verified":objects.len(),"payload_bytes_verified":payload.map(|_|scale),"all_ids_and_payload_hashes_match":true});
        }
        fs::write(
            &args[1],
            serde_json::to_vec_pretty(
                &json!({"schema":"graft-e2ee-load-v1","profile":profile,"scenario":scenario,"scale":scale,"build_profile":if cfg!(debug_assertions){"debug"}else{"release"},"native_crypto_enabled":cfg!(feature="native-crypto"),"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"plaintext_budget_bytes":maximum,"spool_budget_bytes":256*1024*1024,"kdf_create_ms":kdf_create,"kdf_unlock_ms":kdf_unlock,"samples":samples,"evidence":evidence,"limitations":["loopback HTTP and in-memory backend; no production RTT/TLS","core synchronization verifies canonical cache; attachment worktree materialization is excluded","no SQLite/WAL workload in this harness","contention orphan pack bytes include pack-sharing overhead; no deletion performed"]}),
            )?,
        )?;
        Ok(())
    }
}
#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    benchmark::run()
}
#[cfg(not(unix))]
fn main() {
    eprintln!("This diagnostic requires Unix private stores");
    std::process::exit(1);
}
