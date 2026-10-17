//! Synthetic paired HTTP benchmark. Run with --release for performance evidence.
#[cfg(unix)]
mod benchmark {
    use graft_sdk::{
        EncryptedTransferBudget, RemoteConfig, RemoteCredentials, RepositorySession,
        e2ee::{
            EncryptedConfig, FileCheckpointStore, FileMultipartJournalProvider,
            FileRefMutationJournalProvider, RepositoryId, RootKey, SecretSession, SessionBinding,
        },
    };
    use rusqlite::Connection;
    use serde_json::{Value, json};
    use std::{
        fs,
        io::{BufRead, BufReader, Write},
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::{Child, ChildStdin, ChildStdout, Command, Stdio},
        time::Instant,
    };
    use zeroize::Zeroizing;
    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
    const UUID: &str = "00010203-0405-4607-8809-0a0b0c0d0e0f";
    fn event(event: &str, operation: &str) -> Result<()> {
        let handshake = std::env::var("GRAFT_BENCHMARK_RESOURCE_HANDSHAKE").as_deref() == Ok("1");
        println!(
            "{}",
            json!({"event":event,"operation":operation,"resource_handshake":handshake})
        );
        if handshake {
            std::io::stdout().flush()?;
            let mut acknowledgement = String::new();
            std::io::stdin().read_line(&mut acknowledgement)?;
            if acknowledgement != "continue\n" {
                return Err("invalid resource acknowledgement".into());
            }
        }
        Ok(())
    }
    fn identity() -> RepositoryId {
        RepositoryId::from_bytes([0, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
            .unwrap()
    }
    fn private(root: &Path, name: &str) -> Result<PathBuf> {
        let path = root.join(name);
        fs::create_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(path)
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
        fn snapshot(&mut self) -> Result<Value> {
            writeln!(self.input, "snapshot")?;
            self.input.flush()?;
            self.read()
        }
        fn measure(
            &mut self,
            label: &str,
            operation: impl FnOnce() -> Result<()>,
        ) -> Result<Value> {
            let before = self.snapshot()?;
            event("begin", label)?;
            let start = Instant::now();
            operation()?;
            let milliseconds = start.elapsed().as_secs_f64() * 1000.0;
            event("end", label)?;
            let mut metrics = self.snapshot()?;
            metrics["milliseconds"] = json!(milliseconds);
            metrics["resource_operation_name"] = json!(label);
            metrics["server_cpu_microseconds"] = json!({"user":metrics["server_cpu"]["user"].as_u64().unwrap()-before["server_cpu"]["user"].as_u64().unwrap(),"system":metrics["server_cpu"]["system"].as_u64().unwrap()-before["server_cpu"]["system"].as_u64().unwrap()});
            Ok(metrics)
        }
    }
    fn credentials(
        root: &Path,
        label: &str,
        remote: &RemoteConfig,
        config: &EncryptedConfig,
    ) -> Result<RemoteCredentials> {
        let credentials = RemoteCredentials::explicit();
        credentials.set_http_bearer_token("origin", "benchmark-token".into())?;
        if matches!(remote, RemoteConfig::EncryptedHttp { .. }) {
            let binding =
                SessionBinding::new("benchmark.test".into(), "benchmark".into(), identity())?;
            let checkpoint = FileCheckpointStore::create(
                private(root, &format!("{label}-checkpoint"))?,
                &binding,
            )?;
            let secret =
                SecretSession::with_checkpoint_store(binding, std::sync::Arc::new(checkpoint))?;
            secret.unlock_with_root(
                config,
                RootKey::import(identity(), Zeroizing::new([42; 32])),
            )?;
            let refs = FileRefMutationJournalProvider::create(
                private(root, &format!("{label}-refs"))?,
                secret.binding(),
            )?;
            let uploads = FileMultipartJournalProvider::create(
                private(root, &format!("{label}-uploads"))?,
                secret.binding(),
            )?;
            credentials.set_encrypted_session_with_recovery(
                "origin",
                remote,
                secret,
                EncryptedTransferBudget {
                    private_directory: private(root, &format!("{label}-staging"))?,
                    max_buffered_plaintext: 4 * 1024 * 1024,
                    max_ciphertext_spool: 32 * 1024 * 1024,
                },
                std::sync::Arc::new(refs),
                std::sync::Arc::new(uploads),
            )?;
        }
        Ok(credentials)
    }
    fn percentile(values: &mut [f64], fraction: f64) -> f64 {
        values.sort_by(f64::total_cmp);
        values[((values.len() as f64 * fraction).ceil() as usize).saturating_sub(1)]
    }
    pub fn run() -> Result<()> {
        let args: Vec<String> = std::env::args().collect();
        let report = args.get(1).ok_or(
            "usage: e2ee_sync_benchmark REPORT.json [rounds] [plain-first|encrypted-first]",
        )?;
        let rounds: usize = args.get(2).map_or(Ok(5), |value| value.parse())?;
        if !(1..=100).contains(&rounds) {
            return Err("rounds must be 1..100".into());
        }
        let encrypted_first = match args.get(3).map_or("plain-first", String::as_str) {
            "plain-first" => false,
            "encrypted-first" => true,
            _ => return Err("invalid pair order".into()),
        };
        let root = tempfile::tempdir()?;
        let config = EncryptedConfig::create(
            &RootKey::import(identity(), Zeroizing::new([42; 32])),
            b"synthetic benchmark password",
        )?;
        let fixture = root.path().join("fixture.json");
        fs::write(
            &fixture,
            serde_json::to_vec(
                &json!({"resourceMetrics":true,"config":config.as_bytes(), "encryption":{
            "profile":"encrypted-remote-v1", "version":1,"suite":1,"uuid":UUID }}),
            )?,
        )?;
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut child = Command::new("node")
            .arg(workspace.join("tests/e2ee/benchmark-service.mjs"))
            .arg(fixture)
            .current_dir(&workspace)
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
            .ok_or("missing service origin")?
            .to_owned();
        let mut samples = Vec::new();
        for profile in if encrypted_first {
            ["encrypted", "plain"]
        } else {
            ["plain", "encrypted"]
        } {
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
            let source = private(root.path(), &format!("{profile}-source"))?;
            let writer = RepositorySession::new_with_remote_credentials(
                &source,
                None,
                credentials(root.path(), &format!("{profile}-source"), &remote, &config)?,
            );
            writer.open()?;
            writer.init()?;
            writer.add_remote_with_config("origin", remote.clone())?;
            let mut database = Connection::open(source.join("data.db"))?;
            database.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(id INTEGER PRIMARY KEY,value TEXT,counter INTEGER);")?;
            let transaction = database.transaction()?;
            for row in 1..=1024 {
                transaction.execute(
                    "INSERT INTO data VALUES(?1,?2,0)",
                    rusqlite::params![row, "x".repeat(512)],
                )?;
            }
            transaction.commit()?;
            fs::write(source.join("attachment.bin"), vec![7; 256 * 1024])?;
            writer.add_all()?;
            writer.commit("benchmark base")?;
            writer.push(Some("origin"), Some("main"))?;
            let mut receiver = None;
            let mut target = PathBuf::new();
            for round in 0..=rounds {
                target = private(root.path(), &format!("{profile}-clone-{round}"))?;
                let reader = RepositorySession::new_with_remote_credentials(
                    &target,
                    None,
                    credentials(
                        root.path(),
                        &format!("{profile}-clone-{round}"),
                        &remote,
                        &config,
                    )?,
                );
                reader.open()?;
                let metric = service.measure(&format!("{profile}/cold_clone/{round}"), || {
                    reader.clone_repository_with_config(remote.clone(), None)?;
                    Ok(())
                })?;
                if round > 0 {
                    samples.push(json!({"profile":profile,"operation":"cold_clone","round":round,"metrics":metric}));
                }
                if round == rounds {
                    receiver = Some(reader);
                } else {
                    reader.close()?;
                }
            }
            let reader = receiver.unwrap();
            for round in 0..=rounds {
                let counter = round + 1;
                database.execute("UPDATE data SET counter=?1 WHERE id=1", [counter as i64])?;
                writer.add_all()?;
                writer.commit(&format!("benchmark update {counter}"))?;
                let push =
                    service.measure(&format!("{profile}/warm_small_push/{round}"), || {
                        writer.push(Some("origin"), Some("main"))?;
                        Ok(())
                    })?;
                let pull =
                    service.measure(&format!("{profile}/warm_small_pull/{round}"), || {
                        reader.pull(Some("origin"), Some("main"))?;
                        Ok(())
                    })?;
                assert_eq!(
                    reader.repository_metadata()?.current_head,
                    writer.repository_metadata()?.current_head
                );
                let actual: i64 = Connection::open(target.join("data.db"))?.query_row(
                    "SELECT counter FROM data WHERE id=1",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(actual, counter as i64);
                assert_eq!(
                    fs::read(target.join("attachment.bin"))?,
                    vec![7; 256 * 1024]
                );
                if round > 0 {
                    samples.push(json!({"profile":profile,"operation":"warm_small_push","round":round,"metrics":push}));
                    samples.push(json!({"profile":profile,"operation":"warm_small_pull","round":round,"metrics":pull}));
                }
            }
            reader.close()?;
            writer.close()?;
            // A cold update retains canonical local history but uses a new host
            // session, HTTP admission and private checkpoint/cache for each run.
            for round in 0..=rounds {
                let writer = RepositorySession::new_with_remote_credentials(
                    &source,
                    None,
                    credentials(
                        root.path(),
                        &format!("{profile}-cold-writer-{round}"),
                        &remote,
                        &config,
                    )?,
                );
                writer.open()?;
                let reader = RepositorySession::new_with_remote_credentials(
                    &target,
                    None,
                    credentials(
                        root.path(),
                        &format!("{profile}-cold-reader-{round}"),
                        &remote,
                        &config,
                    )?,
                );
                reader.open()?;
                let counter = rounds + round + 2;
                database.execute("UPDATE data SET counter=?1 WHERE id=1", [counter as i64])?;
                writer.add_all()?;
                writer.commit(&format!("benchmark cold update {counter}"))?;
                let push =
                    service.measure(&format!("{profile}/cold_small_push/{round}"), || {
                        writer.push(Some("origin"), Some("main"))?;
                        Ok(())
                    })?;
                let pull =
                    service.measure(&format!("{profile}/cold_small_pull/{round}"), || {
                        reader.pull(Some("origin"), Some("main"))?;
                        Ok(())
                    })?;
                assert_eq!(
                    reader.repository_metadata()?.current_head,
                    writer.repository_metadata()?.current_head
                );
                let actual: i64 = Connection::open(target.join("data.db"))?.query_row(
                    "SELECT counter FROM data WHERE id=1",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(actual, counter as i64);
                assert_eq!(
                    fs::read(target.join("attachment.bin"))?,
                    vec![7; 256 * 1024]
                );
                if round > 0 {
                    samples.push(json!({"profile":profile,"operation":"cold_small_push","round":round,"metrics":push}));
                    samples.push(json!({"profile":profile,"operation":"cold_small_pull","round":round,"metrics":pull}));
                }
                reader.close()?;
                writer.close()?;
            }
        }
        let mut summaries = Vec::new();
        for profile in ["plain", "encrypted"] {
            for operation in [
                "cold_clone",
                "warm_small_push",
                "warm_small_pull",
                "cold_small_push",
                "cold_small_pull",
            ] {
                let group: Vec<&Value> = samples
                    .iter()
                    .filter(|sample| {
                        sample["profile"] == profile && sample["operation"] == operation
                    })
                    .collect();
                let mut summary =
                    json!({"profile":profile,"operation":operation,"samples":group.len()});
                for field in [
                    "milliseconds",
                    "requests",
                    "request_body_bytes",
                    "response_body_bytes",
                ] {
                    let mut values: Vec<f64> = group
                        .iter()
                        .map(|sample| sample["metrics"][field].as_f64().unwrap())
                        .collect();
                    summary[field] = json!({"p50":percentile(&mut values,0.5), "p95":percentile(&mut values,0.95)});
                }
                summaries.push(summary);
            }
        }
        fs::write(
            report,
            serde_json::to_vec_pretty(&json!({"schema":"graft-e2ee-paired-sync-benchmark-v1",
            "build_profile":if cfg!(debug_assertions) {"debug"} else {"release"}, "os":std::env::consts::OS,"arch":std::env::consts::ARCH,
            "native_crypto_enabled":cfg!(feature="native-crypto"),
            "pair_order":if encrypted_first {"encrypted-first"} else {"plain-first"},"rounds":rounds,"warmup_rounds":1,
            "percentile_method":"nearest_rank", "transport":"loopback HTTP; in-memory TypeScript backend; body bytes exclude headers/TLS",
            "dataset":{"sqlite_rows":1024,"row_payload_bytes":512,"attachment_bytes":256*1024,"updated_rows":1},
            "not_measured":["CPU","peak_memory","KDF_latency","production_RTT","orphan_occupancy","other_required_scenarios"],
            "summaries":summaries,"samples":samples}))?,
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
    eprintln!("This benchmark requires the native Unix checkpoint provider");
    std::process::exit(1);
}
