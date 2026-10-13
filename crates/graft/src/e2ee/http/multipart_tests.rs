use super::*;
use crate::e2ee::{
    ChunkSize, FileMultipartJournal, FileMultipartJournalProvider, MultipartJournalProvider,
    ObjectSnapshot, SessionBinding, tests::fixture_root,
};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    sync::{Mutex, OnceLock},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn config() -> EncryptedConfig {
    static CONFIG: OnceLock<EncryptedConfig> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            EncryptedConfig::create(&fixture_root(), b"synthetic multipart transport password")
                .unwrap()
        })
        .clone()
}
fn session() -> SecretSession {
    let session = SecretSession::new(
        SessionBinding::new(
            "multipart.test".into(),
            "account".into(),
            fixture_root().repository(),
        )
        .unwrap(),
        None,
    )
    .unwrap();
    session.unlock_with_root(&config(), fixture_root()).unwrap();
    session
}
fn private_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    directory
}
async fn prepare(session: &SecretSession, directory: &Path) -> PreparedEncryptedObject {
    let bytes = vec![42; 1024];
    let mut source = bytes.as_slice();
    PreparedEncryptedObject::prepare(
        &session.begin_operation().unwrap(),
        ObjectSnapshot {
            logical_key: "store/files/private-multipart-file".into(),
            plaintext_length: bytes.len() as u64,
            plaintext_sha256: Sha256::digest(&bytes).into(),
            chunk_size: ChunkSize::KiB16,
        },
        &mut source,
        directory,
        100_000,
    )
    .await
    .unwrap()
}
fn client(url: &str, session: SecretSession) -> EncryptedHttpClient {
    EncryptedHttpClient::new(url, "origin".into(), RemoteCredentials::explicit(), session).unwrap()
}

#[derive(Default)]
struct State {
    starts: Vec<serde_json::Value>,
    parts: BTreeMap<u64, Vec<u8>>,
    sent_parts: Vec<u64>,
    winner: Option<Vec<u8>>,
    lose_part_once: bool,
    lose_complete: bool,
    tamper_identity: bool,
    tamper_winner: bool,
    change_session: bool,
    start_precondition: bool,
    lose_abort_once: bool,
    aborts: usize,
}
struct Server {
    url: String,
    state: Arc<Mutex<State>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn server(state: State) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/alice/repo", listener.local_addr().unwrap());
    let state = Arc::new(Mutex::new(state));
    let shared = state.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                assert!(header.len() < 16_384);
                header.push(socket.read_u8().await.unwrap());
            }
            let header = String::from_utf8(header).unwrap();
            let path = header
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap();
            let headers: BTreeMap<_, _> = header
                .lines()
                .skip(1)
                .filter_map(|line| line.split_once(':'))
                .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
                .collect();
            let length = headers
                .get("content-length")
                .map_or(0, |v| v.parse::<usize>().unwrap());
            assert!(length < 100_000);
            let mut body = vec![0; length];
            socket.read_exact(&mut body).await.unwrap();
            let (status, extra, bytes) = {
                let mut state = shared.lock().unwrap();
                if path == "/alice/repo" {
                    (200, String::new(), serde_json::to_vec(&serde_json::json!({
                        "protocol":"graft-remote", "version":1,
                        "encryption":{"profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":uuid_text(fixture_root().repository())},
                        "capabilities":["range","list","list-cursor","put-if-absent","cas","cad","e2ee-read-bundle","e2ee-multipart-object"],
                        "limits":{"max_request_bytes":512,"multipart_part_bytes":128}
                    })).unwrap())
                } else if path.ends_with("/raw/e2ee/config") {
                    (200, String::new(), config().as_bytes().to_vec())
                } else if path.contains("/multipart-start/") {
                    let mut identity = serde_json::json!({"uuid":headers["x-graft-e2ee-uuid"],"upload_id":headers["x-graft-e2ee-upload-id"],"ciphertext_sha256":headers["x-graft-e2ee-ciphertext-sha256"]});
                    state.starts.push(identity.clone());
                    if state.tamper_identity {
                        identity["upload_id"] = "00".repeat(32).into();
                    }
                    let reply = serde_json::json!({"upload_id":if state.change_session {"different-server-session"} else {"fixed-server-session"},"total_bytes":headers["x-graft-object-bytes"].parse::<u64>().unwrap(),"part_bytes":128,"uploaded_parts":state.parts.iter().map(|(number,part)| serde_json::json!({"part_number":number,"bytes":part.len()})).collect::<Vec<_>>(),"encryption":identity});
                    (
                        if state.start_precondition { 412 } else { 200 },
                        String::new(),
                        serde_json::to_vec(&reply).unwrap(),
                    )
                } else if path.contains("/multipart-part/") {
                    assert_eq!(
                        headers["x-graft-upload-id"],
                        if state.change_session {
                            "different-server-session"
                        } else {
                            "fixed-server-session"
                        }
                    );
                    let number = headers["x-graft-part-number"].parse().unwrap();
                    state.sent_parts.push(number);
                    state.parts.insert(number, body);
                    let lose = std::mem::take(&mut state.lose_part_once);
                    (if lose { 0 } else { 204 }, String::new(), Vec::new())
                } else if path.contains("/multipart-abort/") {
                    state.aborts += 1;
                    state.change_session = true;
                    // Retain a synthetic claimed part in the new session: restart
                    // must resend every part, never trust pre-abort acknowledgement.
                    let lose = std::mem::take(&mut state.lose_abort_once);
                    (if lose { 0 } else { 204 }, String::new(), Vec::new())
                } else if path.contains("/multipart-complete/") {
                    let mut wire: Vec<_> = state.parts.values().flatten().copied().collect();
                    if state.tamper_winner {
                        *wire.last_mut().unwrap() ^= 1;
                    }
                    state.winner = Some(wire);
                    (
                        if state.lose_complete { 0 } else { 204 },
                        String::new(),
                        Vec::new(),
                    )
                } else if path.contains("/raw/e2ee/") {
                    if let Some(wire) = &state.winner {
                        let range = headers["range"].strip_prefix("bytes=").unwrap();
                        let (begin, end) = range.split_once('-').unwrap();
                        let begin = begin.parse::<usize>().unwrap();
                        let end = end.parse::<usize>().unwrap() + 1;
                        (
                            206,
                            format!(
                                "Content-Range: bytes {}-{}/{}\r\n",
                                begin,
                                end - 1,
                                wire.len()
                            ),
                            wire[begin..end].to_vec(),
                        )
                    } else {
                        (404, String::new(), Vec::new())
                    }
                } else {
                    panic!("unexpected multipart test path {path}");
                }
            };
            if status != 0 {
                let head = format!(
                    "HTTP/1.1 {status} Test\r\nGraft-Protocol: 1\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(&bytes).await.unwrap();
            }
        }
    });
    Server { url, state, task }
}

#[tokio::test]
async fn multipart_explicit_restart_requires_confirmed_abort_and_resends_all_original_ciphertext() {
    let server = server(State {
        lose_part_once: true,
        lose_abort_once: true,
        ..Default::default()
    })
    .await;
    let directory = private_directory();
    let staging = private_directory();
    let session = session();
    let provider = Arc::new(
        FileMultipartJournalProvider::create(directory.path(), session.binding()).unwrap(),
    );
    let candidate = prepare(&session, staging.path()).await;
    let remote = client(&server.url, session.clone());
    assert!(matches!(
        remote
            .publish_object_multipart(&candidate, provider.clone(), staging.path())
            .await
            .unwrap(),
        ObjectPublicationOutcome::OutcomeUnknown { .. }
    ));
    assert!(matches!(
        remote
            .restart_object_multipart(&candidate, provider.clone(), staging.path())
            .await
            .unwrap(),
        ObjectPublicationOutcome::OutcomeUnknown { .. }
    ));
    let path = provider
        .directory(session.binding(), candidate.remote_key())
        .unwrap();
    let lease = FileMultipartJournal::open(&path, session.binding())
        .unwrap()
        .acquire(session.binding())
        .unwrap();
    let record = lease.record.as_ref().unwrap();
    assert_eq!(record.session_id.as_deref(), Some("fixed-server-session"));
    assert!(record.aborted_session_id.is_none());
    drop(lease);
    assert_eq!(server.state.lock().unwrap().sent_parts, vec![1]);
    assert!(matches!(
        remote
            .publish_object_multipart(&candidate, provider.clone(), staging.path())
            .await
            .unwrap(),
        ObjectPublicationOutcome::OutcomeUnknown {
            error: Some(EncryptedHttpError::ProtocolMismatch)
        }
    ));
    assert!(matches!(
        remote
            .restart_object_multipart(&candidate, provider, staging.path())
            .await
            .unwrap(),
        ObjectPublicationOutcome::Created
    ));
    let state = server.state.lock().unwrap();
    assert_eq!(state.aborts, 2);
    assert_eq!(
        state
            .sent_parts
            .iter()
            .filter(|number| **number == 1)
            .count(),
        2
    );
    assert!(state.starts.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(
        Sha256::digest(state.winner.as_ref().unwrap())[..],
        *candidate.ciphertext_sha256()
    );
}

#[tokio::test]
async fn multipart_restarts_reuse_original_ciphertext_skip_received_parts_and_readback_lost_completion()
 {
    let server = server(State {
        lose_part_once: true,
        lose_complete: true,
        ..Default::default()
    })
    .await;
    let directory = private_directory();
    let staging = private_directory();
    let first = session();
    let provider =
        Arc::new(FileMultipartJournalProvider::create(directory.path(), first.binding()).unwrap());
    let original = prepare(&first, staging.path()).await;
    let digest = *original.ciphertext_sha256();
    let remote = client(&server.url, first.clone());
    assert!(
        remote
            .requires_multipart(original.ciphertext_length())
            .await
            .unwrap()
    );
    assert!(matches!(
        remote
            .publish_object_multipart(&original, provider.clone(), staging.path())
            .await
            .unwrap(),
        ObjectPublicationOutcome::OutcomeUnknown { .. }
    ));
    assert_eq!(server.state.lock().unwrap().sent_parts, vec![1]);
    first.disconnect().await;
    drop(remote);
    drop(provider);
    let next = session();
    let provider =
        Arc::new(FileMultipartJournalProvider::open(directory.path(), next.binding()).unwrap());
    let retry = prepare(&next, staging.path()).await;
    assert_ne!(retry.ciphertext_sha256(), &digest);
    let remote = client(&server.url, next.clone());
    assert!(matches!(
        remote
            .publish_object_multipart(&retry, provider.clone(), staging.path())
            .await
            .unwrap(),
        ObjectPublicationOutcome::Reused
    ));
    {
        let state = server.state.lock().unwrap();
        assert_eq!(state.starts.len(), 2);
        assert_eq!(state.starts[0], state.starts[1]);
        assert_eq!(
            state.sent_parts.iter().filter(|part| **part == 1).count(),
            1
        );
        assert_eq!(Sha256::digest(state.winner.as_ref().unwrap())[..], digest);
    }
    let path = provider
        .directory(next.binding(), retry.remote_key())
        .unwrap();
    assert!(
        FileMultipartJournal::open(path, next.binding())
            .unwrap()
            .acquire(next.binding())
            .unwrap()
            .record
            .unwrap()
            .confirmed
    );
    assert!(matches!(
        remote
            .publish_object_multipart(&retry, provider, staging.path())
            .await
            .unwrap(),
        ObjectPublicationOutcome::Reused
    ));
    assert_eq!(server.state.lock().unwrap().starts.len(), 2);
}

#[tokio::test]
async fn multipart_mismatched_identity_or_session_cannot_send_parts() {
    for change_session in [false, true] {
        let server = server(State {
            lose_part_once: change_session,
            tamper_identity: !change_session,
            ..Default::default()
        })
        .await;
        let directory = private_directory();
        let staging = private_directory();
        let session = session();
        let provider = Arc::new(
            FileMultipartJournalProvider::create(directory.path(), session.binding()).unwrap(),
        );
        let candidate = prepare(&session, staging.path()).await;
        let remote = client(&server.url, session);
        assert!(matches!(
            remote
                .publish_object_multipart(&candidate, provider.clone(), staging.path())
                .await
                .unwrap(),
            ObjectPublicationOutcome::OutcomeUnknown { .. }
        ));
        if change_session {
            server.state.lock().unwrap().change_session = true;
            assert!(matches!(
                remote
                    .publish_object_multipart(&candidate, provider, staging.path())
                    .await
                    .unwrap(),
                ObjectPublicationOutcome::OutcomeUnknown {
                    error: Some(EncryptedHttpError::ProtocolMismatch)
                }
            ));
            assert_eq!(server.state.lock().unwrap().sent_parts, vec![1]);
        } else {
            assert!(server.state.lock().unwrap().sent_parts.is_empty());
        }
    }
}

#[tokio::test]
async fn multipart_corrupted_completion_keeps_recovery_ciphertext_and_412_authenticates_full_winner()
 {
    let server = server(State {
        tamper_winner: true,
        ..Default::default()
    })
    .await;
    let directory = private_directory();
    let staging = private_directory();
    let session = session();
    let provider = Arc::new(
        FileMultipartJournalProvider::create(directory.path(), session.binding()).unwrap(),
    );
    let candidate = prepare(&session, staging.path()).await;
    let remote = client(&server.url, session.clone());
    assert!(matches!(
        remote
            .publish_object_multipart(&candidate, provider.clone(), staging.path())
            .await
            .unwrap(),
        ObjectPublicationOutcome::OutcomeUnknown { .. }
    ));
    let path = provider
        .directory(session.binding(), candidate.remote_key())
        .unwrap();
    assert!(
        !FileMultipartJournal::open(&path, session.binding())
            .unwrap()
            .acquire(session.binding())
            .unwrap()
            .record
            .unwrap()
            .confirmed
    );
    assert!(path.join("candidate/ciphertext").is_file());
    server.state.lock().unwrap().start_precondition = true;
    assert!(matches!(
        remote
            .publish_object_multipart(&candidate, provider.clone(), staging.path())
            .await
            .unwrap(),
        ObjectPublicationOutcome::OutcomeUnknown { .. }
    ));
    {
        let mut state = server.state.lock().unwrap();
        *state.winner.as_mut().unwrap().last_mut().unwrap() ^= 1;
    }
    assert!(matches!(
        remote
            .publish_object_multipart(&candidate, provider, staging.path())
            .await
            .unwrap(),
        ObjectPublicationOutcome::Reused
    ));
    assert!(
        FileMultipartJournal::open(path, session.binding())
            .unwrap()
            .acquire(session.binding())
            .unwrap()
            .record
            .unwrap()
            .confirmed
    );
}
