#![cfg(feature = "e2ee")]

use graft::e2ee::{
    ChunkSize, CommitTopology, EncryptedConfig, EncryptedRef, ObjectEncryptor, ObjectReader,
    RefTarget, RepositoryId, RootKey, RoutingHint,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn bytes(value: &Value) -> Vec<u8> {
    let text = value.as_str().unwrap();
    (0..text.len())
        .step_by(2)
        .map(|offset| u8::from_str_radix(&text[offset..offset + 2], 16).unwrap())
        .collect()
}

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/e2ee/independent-v1.json"
    ))
    .unwrap()
}

fn root(fixture: &Value) -> RootKey {
    let repo = fixture["uuid"].as_str().unwrap().replace('-', "");
    RootKey::import(
        RepositoryId::from_bytes(bytes(&Value::String(repo)).try_into().unwrap()).unwrap(),
        Zeroizing::new(bytes(&fixture["root_hex"]).try_into().unwrap()),
    )
}

fn topology(fixture: &Value) -> Vec<CommitTopology> {
    fixture["hint"]["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| CommitTopology {
            commit: bytes(&entry["commit"]).try_into().unwrap(),
            parents: entry["parents"]
                .as_array()
                .unwrap()
                .iter()
                .map(|parent| bytes(parent).try_into().unwrap())
                .collect(),
        })
        .collect()
}

fn target(text: &str) -> RefTarget {
    if let Some(name) = text.strip_prefix("ref: ") {
        RefTarget::Symbolic(name.to_owned())
    } else if let Some(tag) = text.strip_prefix("tag: ") {
        let (object, commit) = tag.split_once(" commit: ").unwrap();
        RefTarget::AnnotatedTag {
            object: bytes(&json!(object)).try_into().unwrap(),
            commit: bytes(&json!(commit)).try_into().unwrap(),
        }
    } else {
        RefTarget::Commit(bytes(&json!(text)).try_into().unwrap())
    }
}

#[test]
fn independent_python_vectors_authenticate_config_objects_refs_hints_and_routes() {
    let fixture = fixture();
    let root = root(&fixture);
    let config = EncryptedConfig::parse(&bytes(&fixture["config"]["wire_hex"])).unwrap();
    config.authenticate(&root).unwrap();
    let unwrapped = config
        .unlock(
            fixture["config"]["password_utf8"]
                .as_str()
                .unwrap()
                .as_bytes(),
            Some(root.repository()),
        )
        .unwrap();
    assert_eq!(
        unwrapped.route_commit(&[7; 32]).unwrap(),
        root.route_commit(&[7; 32]).unwrap()
    );
    assert_eq!(
        hex(&root.route_commit(&[7; 32]).unwrap()),
        fixture["routing"]["route_hex"].as_str().unwrap()
    );
    for entry in fixture["objects"].as_array().unwrap() {
        let wire = bytes(&entry["wire_hex"]);
        let header = 72 + u32::from_be_bytes(wire[68..72].try_into().unwrap()) as usize;
        let reader = ObjectReader::discover(
            &root,
            entry["remote"].as_str().unwrap(),
            &wire[..header],
            wire.len() as u64,
        )
        .unwrap();
        assert_eq!(reader.logical_key(), entry["logical"].as_str().unwrap());
        let expected = bytes(&entry["plaintext_hex"]);
        let size = entry["size"].as_u64().unwrap() as usize;
        let mut offset = header;
        let mut plaintext = Vec::new();
        for index in 0..reader.chunk_count() {
            let length = (expected.len() - plaintext.len()).min(size) + 16;
            plaintext.extend(
                reader
                    .decrypt_chunk(index, &wire[offset..offset + length])
                    .unwrap(),
            );
            let mut tampered = wire[offset..offset + length].to_vec();
            tampered[length - 1] ^= 1;
            assert!(reader.decrypt_chunk(index, &tampered).is_err());
            offset += length;
        }
        assert_eq!(offset, wire.len());
        assert_eq!(plaintext, expected);
        assert_eq!(
            Sha256::digest(&plaintext).as_slice(),
            reader.plaintext_digest()
        );
    }
    for entry in fixture["refs"].as_array().unwrap() {
        let wire = bytes(&entry["wire_hex"]);
        let (_, logical, decoded) =
            EncryptedRef::discover(&root, entry["remote"].as_str().unwrap(), &wire).unwrap();
        assert_eq!(logical, entry["logical"].as_str().unwrap());
        assert!(decoded == target(entry["target"].as_str().unwrap()));
        for offset in 0..wire.len() {
            let mut corrupt = wire.clone();
            corrupt[offset] ^= 1;
            assert!(
                EncryptedRef::discover(&root, entry["remote"].as_str().unwrap(), &corrupt).is_err()
            );
        }
    }
    let hint = &fixture["hint"];
    let wire = bytes(&hint["wire_hex"]);
    RoutingHint::discover(&root, hint["remote"].as_str().unwrap(), &wire)
        .unwrap()
        .verify_topology(&root, &topology(&fixture))
        .unwrap();
    assert_eq!(
        RoutingHint::create(
            &root,
            hint["pack"].as_str().unwrap(),
            hint["index"].as_str().unwrap(),
            &topology(&fixture),
        )
        .unwrap()
        .as_bytes(),
        wire
    );
}

/// Produces randomized native envelopes under PUBLIC SYNTHETIC fixture keys for
/// independent Python verification. Never accepts or exports a user's root.
#[test]
#[ignore = "requires GRAFT_E2EE_VECTOR_OUTPUT pointing to an exclusive scratch file"]
fn export_native_envelopes_for_independent_python_verification() {
    use std::io::Write;

    let mut fixture = fixture();
    let root = root(&fixture);
    let config = EncryptedConfig::create(
        &root,
        fixture["config"]["password_utf8"]
            .as_str()
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    fixture["config"]["wire_hex"] = json!(hex(config.as_bytes()));
    for entry in fixture["objects"].as_array_mut().unwrap() {
        let plaintext = bytes(&entry["plaintext_hex"]);
        let size = match entry["size"].as_u64().unwrap() {
            16384 => ChunkSize::KiB16,
            65536 => ChunkSize::KiB64,
            262144 => ChunkSize::KiB256,
            _ => unreachable!(),
        };
        let mut encryptor = ObjectEncryptor::new(
            &root,
            entry["logical"].as_str().unwrap(),
            plaintext.len() as u64,
            Sha256::digest(&plaintext).into(),
            size,
        )
        .unwrap();
        let mut wire = encryptor.header_bytes().to_vec();
        if plaintext.is_empty() {
            wire.extend(encryptor.encrypt_next(&[]).unwrap());
        } else {
            for chunk in plaintext.chunks(encryptor.chunk_size()) {
                wire.extend(encryptor.encrypt_next(chunk).unwrap());
            }
        }
        encryptor.finish().unwrap();
        entry["wire_hex"] = json!(hex(&wire));
    }
    for entry in fixture["refs"].as_array_mut().unwrap() {
        let reference = EncryptedRef::seal(
            &root,
            entry["logical"].as_str().unwrap(),
            &target(entry["target"].as_str().unwrap()),
        )
        .unwrap();
        entry["wire_hex"] = json!(hex(reference.as_bytes()));
    }
    fixture["generator"] = json!("Rust native randomized envelopes; PUBLIC SYNTHETIC KEYS ONLY");
    // The read bundle remains the deterministic independent framing fixture.
    let path = std::env::var_os("GRAFT_E2EE_VECTOR_OUTPUT").expect("exclusive output path");
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    output
        .write_all(serde_json::to_string_pretty(&fixture).unwrap().as_bytes())
        .unwrap();
}
