//! Physical `SQLite` page hydration, including Graft's compressed segment frame.
//! Metadata is already known; page content is absent from each cold local store.
#[cfg(unix)]
mod benchmark {
    use graft::{
        core::{PageIdx, page::Page},
        e2ee::{
            ChunkSize, EncryptedConfig, EncryptedHttpClient, FileCheckpointStore,
            ObjectPublicationOutcome, ObjectSnapshot, PreparedEncryptedObject, RepositoryId,
            RootKey, SecretSession, SessionBinding,
        },
        local::fjall_storage::FjallStorage,
        remote::{EncryptedTransferBudget, Remote, RemoteConfig, RemoteCredentials},
        rt::runtime::Runtime,
        snapshot::Snapshot,
        volume_reader::{VolumeRead, VolumeReader},
        volume_writer::VolumeWrite,
    };
    use rusqlite::Connection;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::{
        fs,
        io::{BufRead, BufReader, Write},
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::{Child, ChildStdin, ChildStdout, Command, Stdio},
        sync::Arc,
        time::Instant,
    };
    use zeroize::Zeroizing;

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
    const UUID: &str = "00010203-0405-4607-8809-0a0b0c0d0e0f";
    const PAGE_BYTES: usize = 4096;
    const PASSWORD: &[u8] = b"synthetic hydration benchmark password";
    fn identity() -> RepositoryId {
        RepositoryId::from_bytes([0, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
            .unwrap()
    }
    fn private(root: &Path, name: &str) -> Result<PathBuf> {
        let directory = root.join(name);
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        Ok(directory)
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
        fn start(fixture: &Path) -> Result<(Self, String)> {
            let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
            let mut child = Command::new("node")
                .arg(workspace.join("tests/e2ee/benchmark-service.mjs"))
                .arg(fixture)
                .current_dir(workspace)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?;
            let mut service = Self {
                input: child.stdin.take().ok_or("missing service stdin")?,
                output: BufReader::new(child.stdout.take().ok_or("missing service stdout")?),
                child,
            };
            let origin = service.read()?["origin"]
                .as_str()
                .ok_or("missing service origin")?
                .to_owned();
            Ok((service, origin))
        }
        fn read(&mut self) -> Result<Value> {
            let mut line = String::new();
            if self.output.read_line(&mut line)? == 0 {
                return Err("benchmark service exited".into());
            }
            Ok(serde_json::from_str(&line)?)
        }
        fn snapshot(&mut self) -> Result<Value> {
            writeln!(self.input, "snapshot")?;
            self.input.flush()?;
            self.read()
        }
        fn measure<T>(
            &mut self,
            label: &str,
            operation: impl FnOnce() -> Result<T>,
        ) -> Result<(T, Value)> {
            let before = self.snapshot()?;
            resource_event("begin", label)?;
            let start = Instant::now();
            let result = operation()?;
            let milliseconds = start.elapsed().as_secs_f64() * 1000.0;
            resource_event("end", label)?;
            let mut metrics = self.snapshot()?;
            metrics["milliseconds"] = json!(milliseconds);
            metrics["resource_operation_name"] = json!(label);
            metrics["server_cpu_microseconds"] = json!({
                "user":metrics["server_cpu"]["user"].as_u64().ok_or("missing server user CPU")?
                    - before["server_cpu"]["user"].as_u64().ok_or("missing server user CPU")?,
                "system":metrics["server_cpu"]["system"].as_u64().ok_or("missing server system CPU")?
                    - before["server_cpu"]["system"].as_u64().ok_or("missing server system CPU")?});
            metrics["response_amplification"] = json!(
                metrics["response_body_bytes"]
                    .as_u64()
                    .ok_or("missing response bytes")? as f64
                    / PAGE_BYTES as f64
            );
            Ok((result, metrics))
        }
    }
    fn resource_event(event: &str, label: &str) -> Result<()> {
        let enabled = std::env::var("GRAFT_BENCHMARK_RESOURCE_HANDSHAKE").as_deref() == Ok("1");
        println!(
            "{}",
            json!({"event":event,"operation":label,"resource_handshake":enabled})
        );
        std::io::stdout().flush()?;
        if enabled {
            let mut acknowledgement = String::new();
            std::io::stdin().read_line(&mut acknowledgement)?;
            if acknowledgement != "continue\n" {
                return Err("resource sampler acknowledgement missing or malformed".into());
            }
        }
        Ok(())
    }
    fn encrypted_client(
        root: &Path,
        label: &str,
        url: &str,
        config: &EncryptedConfig,
    ) -> Result<(EncryptedHttpClient, SecretSession, PathBuf)> {
        let binding = SessionBinding::new("benchmark.test".into(), "benchmark".into(), identity())?;
        let checkpoint =
            FileCheckpointStore::create(private(root, &format!("{label}-checkpoint"))?, &binding)?;
        let session = SecretSession::with_checkpoint_store(binding, Arc::new(checkpoint))?;
        // Synthetic known root isolates transfer; password KDF is measured separately.
        session.unlock_with_root(
            config,
            RootKey::import(identity(), Zeroizing::new([42; 32])),
        )?;
        let credentials = RemoteCredentials::explicit();
        credentials.set_http_bearer_token("origin", "benchmark-token".into())?;
        let client = EncryptedHttpClient::new(url, "origin".into(), credentials, session.clone())?;
        Ok((client, session, private(root, &format!("{label}-staging"))?))
    }
    fn remote(
        root: &Path,
        label: &str,
        origin: &str,
        encrypted: bool,
        config: &EncryptedConfig,
    ) -> Result<Arc<Remote>> {
        let remote = if encrypted {
            let (client, _, directory) =
                encrypted_client(root, label, &format!("{origin}/encrypted"), config)?;
            Remote::with_encrypted_http(
                client,
                EncryptedTransferBudget {
                    private_directory: directory,
                    max_buffered_plaintext: 4 * 1024 * 1024,
                    max_ciphertext_spool: 32 * 1024 * 1024,
                },
            )?
        } else {
            let credentials = RemoteCredentials::explicit();
            credentials.set_http_bearer_token("origin", "benchmark-token".into())?;
            RemoteConfig::Http {
                url: format!("{origin}/plain"),
                token_env: None,
            }
            .build_with_credentials("origin", &credentials)?
        };
        Ok(Arc::new(remote))
    }
    fn reader(
        handle: &tokio::runtime::Handle,
        remote: Arc<Remote>,
        metadata: Arc<Remote>,
        snapshot: &Snapshot,
    ) -> Result<VolumeReader> {
        let storage = Arc::new(FjallStorage::open_temporary()?);
        let runtime = Runtime::new(handle.clone(), remote, storage, None);
        let (log, lsn) = snapshot.head().ok_or("missing metadata head")?;
        runtime.fetch_log_from(log.clone(), Some(lsn), metadata)?;
        Ok(runtime.snapshot_reader(snapshot.clone()))
    }
    fn sqlite_pages(path: &Path) -> Result<Vec<u8>> {
        let mut database = Connection::open(path)?;
        database.execute_batch("PRAGMA page_size=4096; PRAGMA journal_mode=DELETE; CREATE TABLE data(id INTEGER PRIMARY KEY,value BLOB);")?;
        let mut seed = 0x123456789abcdef0_u64;
        let transaction = database.transaction()?;
        for id in 1..=192 {
            let mut blob = vec![0; 4000];
            for value in &mut blob {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *value = seed as u8;
            }
            transaction.execute(
                "INSERT INTO data VALUES(?1,?2)",
                rusqlite::params![id, blob],
            )?;
        }
        transaction.commit()?;
        let integrity: String =
            database.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err("invalid benchmark SQLite source".into());
        }
        drop(database);
        let bytes = fs::read(path)?;
        if bytes.len() % PAGE_BYTES != 0 || bytes.len() < PAGE_BYTES * 130 {
            return Err("invalid physical page fixture".into());
        }
        Ok(bytes)
    }
    pub fn run() -> Result<()> {
        let args: Vec<_> = std::env::args().collect();
        let output = args
            .get(1)
            .ok_or("usage: e2ee_sqlite_hydration_benchmark REPORT ROUNDS ORDER")?;
        let rounds: usize = args.get(2).map_or(Ok(5), |value| value.parse())?;
        if !(1..=100).contains(&rounds) {
            return Err("rounds must be 1..100".into());
        }
        let profiles = match args.get(3).map_or("plain-first", String::as_str) {
            "plain-first" => ["plain", "encrypted"],
            "encrypted-first" => ["encrypted", "plain"],
            _ => return Err("invalid order".into()),
        };
        let root = tempfile::tempdir()?;
        let config = EncryptedConfig::create(
            &RootKey::import(identity(), Zeroizing::new([42; 32])),
            PASSWORD,
        )?;
        let mut kdf = Vec::new();
        for _ in 0..rounds {
            let start = Instant::now();
            let key = config.unlock(PASSWORD, Some(identity()))?;
            kdf.push(start.elapsed().as_secs_f64() * 1000.0);
            drop(key);
        }
        let fixture = root.path().join("fixture.json");
        fs::write(
            &fixture,
            serde_json::to_vec(&json!({"resourceMetrics":true,"config":config.as_bytes(),
            "encryption":{"profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":UUID}}))?,
        )?;
        let bytes = sqlite_pages(&root.path().join("source.db"))?;
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let memory = Arc::new(RemoteConfig::Memory.build()?);
        let source = Runtime::new(
            tokio.handle().clone(),
            memory.clone(),
            Arc::new(FjallStorage::open_temporary()?),
            None,
        );
        let volume = source.volume_open(None, None, None)?;
        let mut writer = source.volume_writer(volume.vid)?;
        for (index, page) in bytes.chunks_exact(PAGE_BYTES).enumerate() {
            writer.write_page(
                PageIdx::try_from(u32::try_from(index + 1)?)?,
                Page::from_buf(page)?,
            )?;
        }
        let snapshot = writer.commit()?.snapshot().clone();
        source.snapshot_push_to(snapshot.clone(), memory.clone())?;
        let (log, lsn) = snapshot.head().ok_or("missing snapshot head")?;
        let commit = tokio
            .block_on(memory.get_commit(log, lsn))?
            .ok_or("missing source commit")?;
        let sid = commit.segment_id().ok_or("missing segment")?;
        let logical_key = format!("segments/{}", sid.serialize());
        let compressed = tokio
            .block_on(memory.get_raw(&logical_key))?
            .ok_or("missing segment body")?;
        let mut samples = Vec::new();
        let mut allocation = 0usize;
        for (chunk_kib, chunk) in [
            (16, ChunkSize::KiB16),
            (64, ChunkSize::KiB64),
            (256, ChunkSize::KiB256),
        ] {
            let (mut service, origin) = Service::start(&fixture)?;
            let plain = remote(
                root.path(),
                &format!("plain-{chunk_kib}"),
                &origin,
                false,
                &config,
            )?;
            tokio.block_on(plain.put_segment(sid, [compressed.clone()]))?;
            tokio.block_on(plain.put_commit(&commit))?;
            let (client, session, directory) = encrypted_client(
                root.path(),
                &format!("publisher-{chunk_kib}"),
                &format!("{origin}/encrypted"),
                &config,
            )?;
            let operation = session.begin_operation()?;
            let mut input = compressed.as_ref();
            let candidate = tokio.block_on(PreparedEncryptedObject::prepare(
                &operation,
                ObjectSnapshot {
                    logical_key: logical_key.clone(),
                    plaintext_length: compressed.len() as u64,
                    plaintext_sha256: Sha256::digest(&compressed).into(),
                    chunk_size: chunk,
                },
                &mut input,
                &directory,
                2 * 1024 * 1024,
            ))?;
            if !matches!(
                tokio.block_on(client.publish_object(&candidate))?,
                ObjectPublicationOutcome::Created
            ) {
                return Err("fresh segment publication did not create object".into());
            }
            let publisher = Remote::with_encrypted_http(
                client,
                EncryptedTransferBudget {
                    private_directory: directory,
                    max_buffered_plaintext: 4 * 1024 * 1024,
                    max_ciphertext_spool: 32 * 1024 * 1024,
                },
            )?;
            tokio.block_on(publisher.put_commit(&commit))?;
            for profile in profiles {
                let encrypted = profile == "encrypted";
                let warm = remote(
                    root.path(),
                    &format!("warm-{profile}-{chunk_kib}"),
                    &origin,
                    encrypted,
                    &config,
                )?;
                reader(tokio.handle(), warm.clone(), memory.clone(), &snapshot)?
                    .read_page(PageIdx::FIRST)?;
                for (case, number) in [
                    ("first", 1usize),
                    ("frame-boundary", 65),
                    ("tail", bytes.len() / PAGE_BYTES),
                ] {
                    let pageidx = PageIdx::try_from(u32::try_from(number)?)?;
                    let expected = &bytes[(number - 1) * PAGE_BYTES..number * PAGE_BYTES];
                    for mode in ["cold-client", "warm-client"] {
                        for round in 1..=rounds {
                            allocation += 1;
                            let backend = if mode == "cold-client" {
                                remote(
                                    root.path(),
                                    &format!("cold-{allocation}"),
                                    &origin,
                                    encrypted,
                                    &config,
                                )?
                            } else {
                                warm.clone()
                            };
                            let reader =
                                reader(tokio.handle(), backend, memory.clone(), &snapshot)?;
                            for cache in ["uncached-page", "cached-page"] {
                                let operation_name =
                                    format!("{profile}/{chunk_kib}/{case}/{mode}/{cache}");
                                let (page, metrics) = service
                                    .measure(&operation_name, || Ok(reader.read_page(pageidx)?))?;
                                if page.as_ref() != expected {
                                    return Err(
                                        "hydrated SQLite page differs from physical source".into(),
                                    );
                                }
                                if cache == "cached-page" && metrics["requests"] != json!(0) {
                                    return Err("cached page unexpectedly performed HTTP".into());
                                }
                                samples.push(json!({"profile":profile,"operation":operation_name,"round":round,
                                    "chunk_kib":chunk_kib,"case":case,"client":mode,"cache":cache,"metrics":metrics}));
                            }
                        }
                    }
                }
            }
        }
        fs::write(
            output,
            serde_json::to_vec_pretty(&json!({"schema":"graft-e2ee-sqlite-hydration-v1",
            "native_crypto_enabled":cfg!(feature="native-crypto"),
            "rounds":rounds,"samples":samples,"sqlite_bytes":bytes.len(),"compressed_segment_bytes":compressed.len(),
            "password_unlock_milliseconds":kdf,"verified_physical_page_bytes":PAGE_BYTES,
            "limitations":["loopback HTTP and in-memory service; no TLS or production object-store latency",
                "commit metadata is preinstalled identically before timing; actual page content starts absent",
                "each uncached read decompresses the enclosing canonical Graft segment frame, not just 4096 transport bytes",
                "cold clients use fresh session/checkpoint/admission; KDF is excluded from transfer and measured separately",
                "warm clients retain remote admission while local page stores remain fresh",
                "cached page is reread from the same local store and must perform zero HTTP",
                "16/256 KiB envelopes are experimental; normal encrypted segment publication uses 64 KiB",
                "small exploratory sample counts do not establish production tail latency"]}))?,
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
    panic!("requires Unix private encrypted staging");
}
