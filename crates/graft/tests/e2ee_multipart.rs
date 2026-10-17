#![cfg(all(feature = "e2ee", unix))]

use graft::e2ee::{
    ChunkSize, EncryptedConfig, EncryptedHttpClient, FileMultipartJournalProvider,
    ObjectPublicationOutcome, ObjectSnapshot, PreparedEncryptedObject, RepositoryId, RootKey,
    SecretSession, SessionBinding,
};
use graft::remote::RemoteCredentials;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Read},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Stdio},
    sync::Arc,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
struct Service(Child);
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn private(path: &Path) -> Result<()> {
    fs::create_dir(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn session(identity: RepositoryId, config: &EncryptedConfig) -> Result<SecretSession> {
    let session = SecretSession::new(
        SessionBinding::new(
            "multipart-interop.test".into(),
            "synthetic-account".into(),
            identity,
        )?,
        None,
    )?;
    session.unlock_with_root(
        config,
        RootKey::import(identity, zeroize::Zeroizing::new([42; 32])),
    )?;
    Ok(session)
}
fn client(url: &str, session: SecretSession) -> Result<EncryptedHttpClient> {
    let credentials = RemoteCredentials::explicit();
    credentials.set_http_bearer_token("origin", "account-token".into())?;
    Ok(EncryptedHttpClient::new(
        url,
        "origin".into(),
        credentials,
        session,
    )?)
}
async fn prepare(
    session: &SecretSession,
    bytes: &[u8],
    directory: &Path,
) -> Result<PreparedEncryptedObject> {
    let mut source = bytes;
    Ok(PreparedEncryptedObject::prepare(
        &session.begin_operation()?,
        ObjectSnapshot {
            logical_key: "store/files/synthetic-multipart-interop".into(),
            plaintext_length: bytes.len() as u64,
            plaintext_sha256: Sha256::digest(bytes).into(),
            chunk_size: ChunkSize::KiB16,
        },
        &mut source,
        directory,
        1024 * 1024,
    )
    .await?)
}

#[tokio::test]
#[ignore = "requires Node.js, built @eidos.space/graft-remote, and a loopback listener"]
async fn actual_typescript_multipart_handler_resumes_original_ciphertext_after_restart_and_lost_responses()
-> Result<()> {
    run_case(false).await
}

#[tokio::test]
#[ignore = "requires Node.js, built @eidos.space/graft-remote, and a loopback listener"]
async fn actual_typescript_multipart_handler_explicit_abort_preserves_ciphertext_and_resends_all_parts()
-> Result<()> {
    run_case(true).await
}

async fn run_case(restart: bool) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let staging = directory.path().join("staging");
    let recovery = directory.path().join("recovery");
    private(&staging)?;
    private(&recovery)?;
    let identity = RepositoryId::generate()?;
    let root = RootKey::import(identity, zeroize::Zeroizing::new([42; 32]));
    let config = EncryptedConfig::create(&root, b"synthetic multipart interop password")?;
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
    let captures = directory.path().join("capture.json");
    fs::write(
        &fixture,
        serde_json::to_vec(&serde_json::json!({
            "encryption":{"profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":uuid},
            "objects":[["e2ee/config",config.as_bytes()]], "enableMultipart":true,
            "maxRequestBytes":65536,"multipartPartBytes":16384,"multipartFault":"part-and-complete-loss",
        }))?,
    )?;
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut service = Service(
        Command::new("node")
            .arg(workspace.join("tests/e2ee/remote-service.mjs"))
            .arg(fixture)
            .arg(&captures)
            .stdout(Stdio::piped())
            .spawn()?,
    );
    let mut ready = String::new();
    BufReader::new(service.0.stdout.take().ok_or("missing server stdout")?)
        .read_line(&mut ready)?;
    let ready: serde_json::Value = serde_json::from_str(&ready)?;
    let url = ready["url"].as_str().ok_or("missing server URL")?;
    let plaintext: Vec<u8> = (0..131_079).map(|index| (index % 251) as u8).collect();
    let first = session(identity, &config)?;
    let remote = client(url, first.clone())?;
    let provider = Arc::new(FileMultipartJournalProvider::create(
        &recovery,
        first.binding(),
    )?);
    let original = prepare(&first, &plaintext, &staging).await?;
    let digest = *original.ciphertext_sha256();
    assert!(
        remote
            .requires_multipart(original.ciphertext_length())
            .await?
    );
    assert!(matches!(
        remote
            .publish_object_multipart(&original, provider.clone(), &staging)
            .await?,
        ObjectPublicationOutcome::OutcomeUnknown { .. }
    ));
    first.disconnect().await;
    drop(remote);
    drop(provider);
    let next = session(identity, &config)?;
    let remote = client(url, next.clone())?;
    let provider = Arc::new(FileMultipartJournalProvider::open(
        &recovery,
        next.binding(),
    )?);
    let retry = prepare(&next, &plaintext, &staging).await?;
    assert_ne!(retry.ciphertext_sha256(), &digest);
    let outcome = if restart {
        // Trusted recovery needs only the opaque key and enrolled provider; the
        // original plaintext and prepared candidate may no longer exist.
        remote
            .restart_retained_object_multipart(original.remote_key(), provider, &staging)
            .await?
    } else {
        remote
            .publish_object_multipart(&retry, provider, &staging)
            .await?
    };
    assert!(matches!(outcome, ObjectPublicationOutcome::Reused));
    let verified = remote
        .download_object(
            "store/files/synthetic-multipart-interop",
            &staging,
            plaintext.len() as u64,
        )
        .await?
        .ok_or("missing verified winner")?;
    let mut downloaded = Vec::new();
    verified.into_file().read_to_end(&mut downloaded)?;
    assert_eq!(downloaded, plaintext);
    let capture: Vec<serde_json::Value> = serde_json::from_slice(&fs::read(captures)?)?;
    let starts: Vec<_> = capture
        .iter()
        .filter(|item| item["path"].as_str().unwrap().contains("/multipart-start/"))
        .collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(
        starts[0]["encryptionUploadId"],
        starts[1]["encryptionUploadId"]
    );
    assert_eq!(
        starts[0]["ciphertextSha256"],
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    assert_eq!(starts[0]["ciphertextSha256"], starts[1]["ciphertextSha256"]);
    assert_eq!(
        capture
            .iter()
            .filter(|item| item["partNumber"] == "1")
            .count(),
        if restart { 2 } else { 1 }
    );
    assert_eq!(
        capture
            .iter()
            .filter(
                |item| item["path"].as_str().unwrap().contains("/multipart-abort/")
                    && item["responseStatus"] == 204
            )
            .count(),
        usize::from(restart)
    );
    assert_eq!(
        capture
            .iter()
            .filter(|item| item["droppedMultipartResponse"] == true)
            .count(),
        2
    );
    assert!(capture.iter().any(
        |item| item["path"].as_str().unwrap().contains("/raw/e2ee/files/")
            && item["responseStatus"] == 206
    ));
    next.disconnect().await;
    Ok(())
}
