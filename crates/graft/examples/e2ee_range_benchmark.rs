//! Authenticated transport range benchmark, not full `SQLite` hydration evidence.
#[cfg(unix)]
mod benchmark {
    use graft::{
        e2ee::{
            ChunkSize, EncryptedConfig, EncryptedHttpClient, FileCheckpointStore, ObjectSnapshot,
            PreparedEncryptedObject, RepositoryId, RootKey, SecretSession, SessionBinding,
        },
        remote::RemoteCredentials,
    };
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
    fn identity() -> RepositoryId {
        RepositoryId::from_bytes([0, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
            .unwrap()
    }
    fn root() -> RootKey {
        RootKey::import(identity(), Zeroizing::new([42; 32]))
    }
    fn private(base: &Path, name: &str) -> Result<PathBuf> {
        let path = base.join(name);
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
    }
    fn encrypted_client(
        base: &Path,
        label: &str,
        url: &str,
        config: &EncryptedConfig,
    ) -> Result<(EncryptedHttpClient, SecretSession)> {
        let binding = SessionBinding::new(
            "range-benchmark.test".into(),
            "benchmark".into(),
            identity(),
        )?;
        let store = FileCheckpointStore::create(private(base, label)?, &binding)?;
        let secret = SecretSession::with_checkpoint_store(binding, Arc::new(store))?;
        secret.unlock_with_root(config, root())?;
        let credentials = RemoteCredentials::explicit();
        credentials.set_http_bearer_token("origin", "benchmark-token".into())?;
        Ok((
            EncryptedHttpClient::new(url, "origin".into(), credentials, secret.clone())?,
            secret,
        ))
    }
    fn plain_client() -> Result<reqwest::Client> {
        Ok(reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .http1_only()
            .pool_max_idle_per_host(1)
            .build()?)
    }
    fn percentile(mut values: Vec<f64>, fraction: f64) -> f64 {
        values.sort_by(f64::total_cmp);
        values[((values.len() as f64 * fraction).ceil() as usize).saturating_sub(1)]
    }
    pub async fn run() -> Result<()> {
        let args: Vec<String> = std::env::args().collect();
        let report = args.get(1).ok_or(
            "usage: e2ee_range_benchmark REPORT.json [rounds] [plain-first|encrypted-first]",
        )?;
        let rounds: usize = args.get(2).map_or(Ok(5), |s| s.parse())?;
        if !(1..=100).contains(&rounds) {
            return Err("rounds must be 1..100".into());
        }
        let reverse = match args.get(3).map_or("plain-first", String::as_str) {
            "plain-first" => false,
            "encrypted-first" => true,
            _ => return Err("invalid pair order".into()),
        };
        let temporary = tempfile::tempdir()?;
        let config = EncryptedConfig::create(&root(), b"synthetic range benchmark password")?;
        let fixture = temporary.path().join("fixture.json");
        fs::write(
            &fixture,
            serde_json::to_vec(&json!({"config":config.as_bytes(),"encryption":{
            "profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":UUID}}))?,
        )?;
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut child = Command::new("node")
            .arg(workspace.join("tests/e2ee/benchmark-service.mjs"))
            .arg(fixture)
            .current_dir(workspace)
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
            .to_owned();
        let plain_url = format!("{origin}/plain");
        let encrypted_url = format!("{origin}/encrypted");
        let payload: Vec<u8> = (0..1024 * 1024 + 123).map(|i| (i % 251) as u8).collect();
        let plain = plain_client()?;
        let mut samples = Vec::new();
        for (chunk, block) in [
            (ChunkSize::KiB16, 16384),
            (ChunkSize::KiB64, 65536),
            (ChunkSize::KiB256, 262144),
        ] {
            let logical = format!("store/files/range-{block}.bin");
            let response = plain
                .put(format!("{plain_url}/raw-if-not-exists/{logical}"))
                .bearer_auth("benchmark-token")
                .header("Graft-Protocol", "1")
                .body(payload.clone())
                .send()
                .await?;
            if !response.status().is_success() {
                return Err("plain publication failed".into());
            }
            let (publisher, secret) = encrypted_client(
                temporary.path(),
                &format!("publisher-{block}"),
                &encrypted_url,
                &config,
            )?;
            let prepared = PreparedEncryptedObject::prepare(
                &secret.begin_operation()?,
                ObjectSnapshot {
                    logical_key: logical.clone(),
                    plaintext_length: payload.len() as u64,
                    plaintext_sha256: Sha256::digest(&payload).into(),
                    chunk_size: chunk,
                },
                &mut payload.as_slice(),
                &private(temporary.path(), &format!("spool-{block}"))?,
                2 * 1024 * 1024,
            )
            .await?;
            publisher.publish_object(&prepared).await?;
            for profile in if reverse {
                ["encrypted", "plain"]
            } else {
                ["plain", "encrypted"]
            } {
                for (case, start) in [
                    ("aligned", 0),
                    ("cross_chunk", block - 2048),
                    ("tail", payload.len() - 4096),
                ] {
                    for mode in ["cold_client", "warm_client"] {
                        let warm_plain = plain_client()?;
                        let (warm_encrypted, _) = encrypted_client(
                            temporary.path(),
                            &format!("warm-{block}-{profile}-{case}-{mode}"),
                            &encrypted_url,
                            &config,
                        )?;
                        for round in 0..=rounds {
                            let cold_plain = plain_client()?;
                            let (cold_encrypted, _) = encrypted_client(
                                temporary.path(),
                                &format!("cold-{block}-{profile}-{case}-{mode}-{round}"),
                                &encrypted_url,
                                &config,
                            )?;
                            let plain = if mode == "cold_client" {
                                &cold_plain
                            } else {
                                &warm_plain
                            };
                            let encrypted = if mode == "cold_client" {
                                &cold_encrypted
                            } else {
                                &warm_encrypted
                            };
                            service.snapshot()?;
                            let timer = Instant::now();
                            let bytes = if profile == "plain" {
                                let response = plain
                                    .get(format!("{plain_url}/raw/{logical}"))
                                    .bearer_auth("benchmark-token")
                                    .header("Graft-Protocol", "1")
                                    .header("Range", format!("bytes={start}-{}", start + 4095))
                                    .send()
                                    .await?;
                                if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                                    return Err("plain range not honored".into());
                                }
                                let expected_range =
                                    format!("bytes {start}-{}/{}", start + 4095, payload.len());
                                if response
                                    .headers()
                                    .get("content-range")
                                    .and_then(|v| v.to_str().ok())
                                    != Some(expected_range.as_str())
                                {
                                    return Err("plain content-range mismatch".into());
                                }
                                response.bytes().await?.to_vec()
                            } else {
                                encrypted
                                    .read_object_range(
                                        &logical,
                                        start as u64..(start + 4096) as u64,
                                    )
                                    .await?
                                    .ok_or("missing encrypted range")?
                            };
                            let milliseconds = timer.elapsed().as_secs_f64() * 1000.0;
                            let mut metrics = service.snapshot()?;
                            if bytes != payload[start..start + 4096] {
                                return Err("range bytes mismatch".into());
                            }
                            metrics["milliseconds"] = json!(milliseconds);
                            metrics["body_read_amplification"] =
                                json!(metrics["response_body_bytes"].as_f64().unwrap() / 4096.0);
                            if round != 0 {
                                samples.push(json!({"profile":profile,"block_bytes":block,"case":case,
                                "mode":mode,"round":round,"offset":start,"logical_bytes":4096,"metrics":metrics}));
                            }
                        }
                    }
                }
            }
        }
        let mut summaries = Vec::new();
        for profile in ["plain", "encrypted"] {
            for block in [16384, 65536, 262144] {
                for case in ["aligned", "cross_chunk", "tail"] {
                    for mode in ["cold_client", "warm_client"] {
                        let group: Vec<&Value> = samples
                            .iter()
                            .filter(|s| {
                                s["profile"] == profile
                                    && s["block_bytes"] == block
                                    && s["case"] == case
                                    && s["mode"] == mode
                            })
                            .collect();
                        let mut summary = json!({"profile":profile,"block_bytes":block,"case":case,"mode":mode,"samples":group.len()});
                        for field in [
                            "milliseconds",
                            "requests",
                            "response_body_bytes",
                            "body_read_amplification",
                        ] {
                            let values: Vec<f64> = group
                                .iter()
                                .map(|s| s["metrics"][field].as_f64().unwrap())
                                .collect();
                            summary[field] = json!({"p50":percentile(values.clone(),0.5),"p95":percentile(values,0.95)});
                        }
                        summaries.push(summary);
                    }
                }
            }
        }
        fs::write(
            report,
            serde_json::to_vec_pretty(&json!({"schema":"graft-e2ee-range-benchmark-v1",
            "build_profile":if cfg!(debug_assertions){"debug"}else{"release"},
            "os":std::env::consts::OS,"arch":std::env::consts::ARCH,"rounds":rounds,"warmup_rounds":1,
            "pair_order":if reverse{"encrypted-first"}else{"plain-first"},"object_bytes":payload.len(),
            "scope":"actual HTTP authenticated range transport; not full SQLite hydration",
            "percentile_method":"nearest_rank",
            "transport":"loopback HTTP; in-memory TypeScript backend; body bytes exclude headers/TLS",
            "timing_excludes":["KDF","local_client_setup","publication","metric_snapshots","returned_byte_comparison"],
            "not_measured":["CPU","peak_memory","KDF_latency","production_RTT","SQLite_hydration"],
            "summaries":summaries,"samples":samples}))?,
        )?;
        Ok(())
    }
}
#[cfg(unix)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    benchmark::run().await
}
#[cfg(not(unix))]
fn main() {
    eprintln!("range benchmark requires Unix private-directory support");
}
