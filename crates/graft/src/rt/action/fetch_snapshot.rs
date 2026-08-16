use std::{collections::HashSet, sync::Arc};

use futures::{StreamExt, TryStreamExt, stream};
use bilrost::OwnedMessage;

use crate::{
    GraftErr, LogicalErr,
    core::{
        LogId,
        cbe::CBE64,
        commit::Commit,
        lsn::{LSN, LSNRangeExt},
    },
    local::fjall_storage::FjallStorage,
    remote::{ReadBundleOutcome, Remote, RemoteErr},
    rt::action::Action,
    snapshot::Snapshot,
};

const SNAPSHOT_COMMIT_FETCH_CONCURRENCY: usize = 5;

/// Repairs every missing commit in a known snapshot through one shared request window.
#[derive(Debug)]
pub struct FetchSnapshot {
    pub snapshot: Snapshot,
}

impl Action for FetchSnapshot {
    async fn run(self, storage: Arc<FjallStorage>, remote: Arc<Remote>) -> Result<(), GraftErr> {
        let fetch_trace = crate::trace::PushTraceSpan::new("sqlite_snapshot_commit_fetch");
        let missing = {
            let reader = storage.read();
            let mut missing = Vec::new();
            for range in self.snapshot.iter() {
                for lsn in range.lsns.iter() {
                    if reader.get_commit(&range.log, lsn)?.is_none() {
                        missing.push((range.log.clone(), lsn));
                    }
                }
            }
            missing
        };
        let requested_commits = missing.len() as u64;

        let mut commits = fetch_commits(remote.clone(), missing.clone()).await?;
        let requested = missing.into_iter().collect::<HashSet<_>>();
        let checkpoint_requests = {
            let reader = storage.read();
            let mut checkpoints = HashSet::new();
            for commit in &commits {
                for &lsn in commit.checkpoints() {
                    let key = (commit.log.clone(), lsn);
                    if !requested.contains(&key)
                        && reader.get_commit(&commit.log, lsn)?.is_none()
                    {
                        checkpoints.insert(key);
                    }
                }
            }
            checkpoints.into_iter().collect::<Vec<_>>()
        };
        let requested_checkpoints = checkpoint_requests.len() as u64;
        commits.extend(fetch_commits(remote, checkpoint_requests).await?);
        let fetched_commits = commits.len() as u64;

        let mut batch = storage.batch();
        for commit in commits {
            batch.write_commit(commit);
        }
        batch.commit()?;
        fetch_trace.finish(&[
            ("requested_commits", requested_commits),
            ("requested_checkpoints", requested_checkpoints),
            ("fetched_commits", fetched_commits),
        ]);
        Ok(())
    }
}

async fn fetch_commits(
    remote: Arc<Remote>,
    requests: Vec<(LogId, LSN)>,
) -> Result<Vec<Commit>, GraftErr> {
    let paths = requests
        .iter()
        .map(|(log, lsn)| {
            format!(
                "logs/{}/commits/{}",
                log.serialize(),
                CBE64::from(*lsn)
            )
        })
        .collect::<Vec<_>>();
    match remote.get_raw_bundle(&paths).await? {
        ReadBundleOutcome::Downloaded(mut objects) => requests
            .into_iter()
            .zip(paths)
            .filter_map(|((log, lsn), path)| {
                objects.remove(&path).map(|bytes| (log, lsn, path, bytes))
            })
            .map(|(log, lsn, path, bytes)| {
                let commit = Commit::decode(bytes).map_err(RemoteErr::from)?;
                if commit.log != log || commit.lsn != lsn {
                    return Err(LogicalErr::Other(format!(
                        "read-bundle returned the wrong commit for {path}"
                    ))
                    .into());
                }
                Ok(commit)
            })
            .collect(),
        ReadBundleOutcome::Unsupported => stream::iter(requests)
            .map(|(log, lsn)| {
                let remote = remote.clone();
                async move { Ok::<_, GraftErr>(remote.get_commit(&log, lsn).await?) }
            })
            .buffer_unordered(SNAPSHOT_COMMIT_FETCH_CONCURRENCY)
            .try_filter_map(|commit| async move { Ok(commit) })
            .try_collect()
            .await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bilrost::Message;
    use crate::{
        core::{PageCount, cbe::CBE64, commit::Commit},
        remote::RemoteConfig,
    };
    use std::{
        collections::HashMap,
        io::{Read, Write},
        net::TcpListener,
        thread,
    };
    use thin_vec::thin_vec;

    #[test]
    fn fetches_multiple_snapshot_ranges_and_referenced_checkpoints() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let remote = Arc::new(RemoteConfig::Memory.build().unwrap());
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        let first_log = LogId::random();
        let second_log = LogId::random();
        let checkpoint = Commit::new(first_log.clone(), LSN::FIRST, PageCount::ZERO);
        let first_head = Commit::new(first_log.clone(), LSN::new(2), PageCount::ZERO)
            .with_checkpoints(thin_vec![LSN::FIRST]);
        let second_head = Commit::new(second_log.clone(), LSN::FIRST, PageCount::ZERO);
        runtime.block_on(async {
            remote.put_commit(&checkpoint).await.unwrap();
            remote.put_commit(&first_head).await.unwrap();
            remote.put_commit(&second_head).await.unwrap();
        });

        let mut snapshot = Snapshot::new(
            first_log.clone(),
            LSN::new(2)..=LSN::new(2),
            PageCount::ZERO,
        );
        snapshot.append(second_log.clone(), LSN::FIRST..=LSN::FIRST);
        runtime
            .block_on(FetchSnapshot { snapshot }.run(storage.clone(), remote))
            .unwrap();

        let reader = storage.read();
        assert!(reader.get_commit(&first_log, LSN::FIRST).unwrap().is_some());
        assert!(reader.get_commit(&first_log, LSN::new(2)).unwrap().is_some());
        assert!(reader.get_commit(&second_log, LSN::FIRST).unwrap().is_some());
    }

    #[test]
    fn snapshot_ranges_use_one_http_read_bundle() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut snapshot = Snapshot::empty();
        let mut responses = HashMap::new();
        for _ in 0..10 {
            let log = LogId::random();
            snapshot.append(log.clone(), LSN::FIRST..=LSN::FIRST);
            let commit = Commit::new(log.clone(), LSN::FIRST, PageCount::ZERO);
            responses.insert(
                format!(
                    "logs/{}/commits/{}",
                    log.serialize(),
                    CBE64::from(LSN::FIRST)
                ),
                commit.encode_to_bytes(),
            );
        }
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut headers = Vec::new();
            let mut byte = [0_u8; 1];
            while !headers.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            let headers = String::from_utf8(headers).unwrap();
            assert!(headers.starts_with("POST /repo/read-bundle HTTP/1.1"));
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::parse::<usize>)
                })
                .unwrap()
                .unwrap();
            let mut manifest = vec![0; content_length];
            stream.read_exact(&mut manifest).unwrap();
            let manifest: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
            assert_eq!(manifest["paths"].as_array().unwrap().len(), 10);

            let mut body = Vec::new();
            let mut objects = responses.into_iter().collect::<Vec<_>>();
            objects.sort_by(|left, right| left.0.cmp(&right.0));
            for (path, bytes) in objects {
                body.extend_from_slice(&(path.len() as u32).to_be_bytes());
                body.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
                body.extend_from_slice(path.as_bytes());
                body.extend_from_slice(&bytes);
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nGraft-Protocol: 1\r\nConnection: close\r\nX-Graft-Bundle-Objects: 10\r\nX-Graft-Bundle-Total-Bytes: {}\r\nContent-Length: {}\r\n\r\n",
                body.len(),
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
            stream.flush().unwrap();
        });

        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let remote = Arc::new(
            RemoteConfig::Http {
                url: format!("http://{address}/repo"),
                token_env: None,
            }
            .build()
            .unwrap(),
        );
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        tokio
            .block_on(FetchSnapshot { snapshot }.run(storage, remote))
            .unwrap();
        server.join().unwrap();
    }

    #[test]
    fn snapshot_ranges_fall_back_for_an_older_http_remote() {
        fn read_request(stream: &mut std::net::TcpStream) -> String {
            let mut headers = Vec::new();
            let mut byte = [0_u8; 1];
            while !headers.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            let headers = String::from_utf8(headers).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::parse::<usize>)
                })
                .transpose()
                .unwrap()
                .unwrap_or(0);
            let mut body = vec![0; content_length];
            stream.read_exact(&mut body).unwrap();
            headers
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut snapshot = Snapshot::empty();
        let mut responses = HashMap::new();
        let mut expected = Vec::new();
        for _ in 0..3 {
            let log = LogId::random();
            snapshot.append(log.clone(), LSN::FIRST..=LSN::FIRST);
            expected.push((log.clone(), LSN::FIRST));
            let path = format!(
                "logs/{}/commits/{}",
                log.serialize(),
                CBE64::from(LSN::FIRST)
            );
            responses.insert(
                format!("/repo/raw/{path}"),
                Commit::new(log, LSN::FIRST, PageCount::ZERO).encode_to_bytes(),
            );
        }
        let server = thread::spawn(move || {
            let (mut bundle, _) = listener.accept().unwrap();
            let request = read_request(&mut bundle);
            assert!(request.starts_with("POST /repo/read-bundle HTTP/1.1"));
            write!(
                bundle,
                "HTTP/1.1 404 Not Found\r\nGraft-Protocol: 1\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
            )
            .unwrap();
            bundle.flush().unwrap();

            for _ in 0..responses.len() {
                let (mut request_stream, _) = listener.accept().unwrap();
                let request = read_request(&mut request_stream);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap();
                let body = responses.get(path).expect("requested snapshot commit");
                write!(
                    request_stream,
                    "HTTP/1.1 200 OK\r\nGraft-Protocol: 1\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                )
                .unwrap();
                request_stream.write_all(body).unwrap();
                request_stream.flush().unwrap();
            }
        });

        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let remote = Arc::new(
            RemoteConfig::Http {
                url: format!("http://{address}/repo"),
                token_env: None,
            }
            .build()
            .unwrap(),
        );
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        tokio
            .block_on(FetchSnapshot { snapshot }.run(storage.clone(), remote))
            .unwrap();
        let reader = storage.read();
        for (log, lsn) in expected {
            assert!(reader.get_commit(&log, lsn).unwrap().is_some());
        }
        server.join().unwrap();
    }
}
