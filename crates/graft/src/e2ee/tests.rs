use super::*;
use sha2::Digest;

#[test]
fn aggregate_manifest_matches_independent_shared_transport_vector() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/e2ee/read-bundle.json"
    ))
    .unwrap();
    let key = fixture["key"].as_str().unwrap().as_bytes();
    let payload_text = fixture["payload_hex"].as_str().unwrap();
    let payload = (0..payload_text.len())
        .step_by(2)
        .map(|offset| u8::from_str_radix(&payload_text[offset..offset + 2], 16).unwrap())
        .collect::<Vec<_>>();
    let length = (payload.len() as u64).to_be_bytes();
    let descriptor = codec::encode(&[key, &length]).unwrap();
    let descriptors = codec::encode(&[&descriptor]).unwrap();
    let manifest = codec::encode(&[&1_u16.to_be_bytes(), &[], &[], &descriptors]).unwrap();
    assert_eq!(hex(&manifest), fixture["manifest_hex"].as_str().unwrap());
    let mut wire = b"GRAFTEB1".to_vec();
    wire.extend_from_slice(&(manifest.len() as u32).to_be_bytes());
    wire.extend_from_slice(&manifest);
    wire.extend_from_slice(&(key.len() as u32).to_be_bytes());
    wire.extend_from_slice(&length);
    wire.extend_from_slice(key);
    wire.extend_from_slice(&payload);
    assert_eq!(hex(&wire), fixture["wire_hex"].as_str().unwrap());
}

#[test]
fn routing_hints_preserve_topology_and_authenticate_all_bytes() {
    let root = fixture_root();
    let pack = format!("e2ee/packs/{}", "a".repeat(64));
    let index = format!("e2ee/indexes/{}", "b".repeat(64));
    let commits = vec![
        CommitTopology { commit: [1; 32], parents: vec![] },
        CommitTopology { commit: [2; 32], parents: vec![[1; 32]] },
        CommitTopology {
            commit: [3; 32],
            parents: vec![[2; 32], [1; 32]],
        },
    ];
    let hint = RoutingHint::create(&root, &pack, &index, &commits).unwrap();
    assert_eq!(hint.remote_key(), format!("e2ee/hints/{}", "a".repeat(64)));
    let opened = RoutingHint::open(&root, &pack, &index, hint.as_bytes()).unwrap();
    opened.verify_topology(&root, &commits).unwrap();
    let mut reordered = commits.clone();
    reordered.reverse();
    assert_eq!(
        RoutingHint::create(&root, &pack, &index, &reordered)
            .unwrap()
            .as_bytes(),
        hint.as_bytes()
    );
    reordered[0].parents.reverse();
    assert!(opened.verify_topology(&root, &reordered).is_err());
    assert!(opened.verify_topology(&root, &commits[..2]).is_err());
    for offset in 0..hint.as_bytes().len() {
        let mut corrupt = hint.as_bytes().to_vec();
        corrupt[offset] ^= 1;
        assert!(RoutingHint::open(&root, &pack, &index, &corrupt).is_err());
        assert!(RoutingHint::open(&root, &pack, &index, &hint.as_bytes()[..offset]).is_err());
    }
    let other_pack = format!("e2ee/packs/{}", "c".repeat(64));
    assert!(RoutingHint::open(&root, &other_pack, &index, hint.as_bytes()).is_err());
    assert!(RoutingHint::open(&root, &pack, &other_pack, hint.as_bytes()).is_err());
    let other = RootKey::generate(RepositoryId::generate().unwrap()).unwrap();
    assert!(RoutingHint::open(&other, &pack, &index, hint.as_bytes()).is_err());
    assert!(opened.verify_topology(&other, &commits).is_err());
    let empty = RoutingHint::create(&root, &pack, &index, &[]).unwrap();
    RoutingHint::open(&root, &pack, &index, empty.as_bytes())
        .unwrap()
        .verify_topology(&root, &[])
        .unwrap();
}

#[test]
fn routing_hint_limits_and_authenticated_malformed_topology_are_rejected() {
    let root = fixture_root();
    let pack = format!("e2ee/packs/{}", "a".repeat(64));
    let index = format!("e2ee/indexes/{}", "b".repeat(64));
    let duplicate = CommitTopology { commit: [1; 32], parents: vec![] };
    assert!(
        RoutingHint::create(
            &root,
            &pack,
            &index,
            &[duplicate.clone(), duplicate.clone()]
        )
        .is_err()
    );
    assert!(
        RoutingHint::create(
            &root,
            &pack,
            &index,
            &[CommitTopology {
                commit: [1; 32],
                parents: vec![[2; 32], [2; 32]]
            }]
        )
        .is_err()
    );
    assert!(
        RoutingHint::create(
            &root,
            &pack,
            &index,
            &[CommitTopology {
                commit: [1; 32],
                parents: vec![[2; 32]; 33]
            }]
        )
        .is_err()
    );
    assert!(RoutingHint::create(&root, &pack, &index, &vec![duplicate; 4097]).is_err());
    assert!(RoutingHint::create(&root, &index, &pack, &[]).is_err());
    assert!(RoutingHint::create(&root, &pack.to_uppercase(), &index, &[]).is_err());
    assert!(matches!(
        RoutingHint::open(&root, &pack, &index, &vec![0; 1024 * 1024 + 1]),
        Err(EncryptionError::TooLarge)
    ));

    let make_entry = |commit: &[u8], parents: &[&[u8]]| {
        codec::encode(&[commit, &codec::encode(parents).unwrap()]).unwrap()
    };
    let empty_parents: &[&[u8]] = &[];
    let valid = make_entry(&[1; 32], empty_parents);
    let wrong_size = make_entry(&[1; 31], empty_parents);
    let duplicate_parents = make_entry(&[1; 32], &[&[2; 32], &[2; 32]]);
    let high = make_entry(&[2; 32], empty_parents);
    // Valid MACs cannot make malformed or unsorted topology acceptable.
    for entries in [
        vec![valid.clone(), valid.clone()],
        vec![high, valid],
        vec![wrong_size],
        vec![duplicate_parents],
    ] {
        let entries =
            codec::encode(&entries.iter().map(Vec::as_slice).collect::<Vec<_>>()).unwrap();
        let body = codec::encode(&[pack.as_bytes(), index.as_bytes(), &entries]).unwrap();
        let mut frame = b"GRAFTEH1".to_vec();
        frame.extend_from_slice(&1_u16.to_be_bytes());
        frame.extend_from_slice(&SUITE.to_be_bytes());
        frame.extend_from_slice(root.repository().as_bytes());
        let key = format!("e2ee/hints/{}", "a".repeat(64));
        let input = codec::encode(&[
            b"graft/routing-hint/v1",
            PROFILE,
            root.repository().as_bytes(),
            key.as_bytes(),
            &frame,
            &body,
        ])
        .unwrap();
        frame.extend_from_slice(&body);
        frame.extend_from_slice(&mac(root.derive(b"hint").unwrap().as_ref(), &input));
        assert!(RoutingHint::open(&root, &pack, &index, &frame).is_err());
    }
}

#[test]
fn annotated_tag_refs_authenticate_exact_object_and_commit_dependency() {
    use crate::repo::{Repository, object::ObjectId};
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    let path = directory.path().join("tag.txt");
    std::fs::write(&path, "tag source").unwrap();
    repo.stage_artifact_path(&path).unwrap();
    let commit = repo.commit_staged("tag source").unwrap().id;
    let first = repo
        .tag_create_annotated("first", Some(&commit), "first annotation")
        .unwrap();
    let second = repo
        .tag_create_annotated("second", Some(&commit), "different annotation")
        .unwrap();
    let bytes = repo
        .object_store()
        .read_raw(&ObjectId::new(&first.object).unwrap())
        .unwrap()
        .unwrap();
    let other = repo
        .object_store()
        .read_raw(&ObjectId::new(&second.object).unwrap())
        .unwrap()
        .unwrap();
    let target = RefTarget::from_annotated_tag(&bytes).unwrap();
    target.verify_annotated_tag(&bytes).unwrap();
    assert!(target.verify_annotated_tag(&other).is_err());
    let commit_bytes = repo
        .object_store()
        .read_raw(&ObjectId::new(&commit).unwrap())
        .unwrap()
        .unwrap();
    assert!(RefTarget::from_annotated_tag(&commit_bytes).is_err());
    let root = fixture_root();
    let logical = "refs/tags/release/中文";
    let sealed = EncryptedRef::seal(&root, logical, &target).unwrap();
    assert_eq!(sealed.as_bytes()[12], 3);
    let RefTarget::AnnotatedTag { object, commit: dependency } = &target else {
        panic!("not an annotated tag");
    };
    assert_eq!(
        object
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
        first.object
    );
    assert_eq!(
        dependency
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
        commit
    );
    assert_eq!(
        &sealed.as_bytes()[13..45],
        root.route_commit(dependency).unwrap()
    );
    let (observed, decoded) = EncryptedRef::open(&root, logical, sealed.as_bytes()).unwrap();
    assert!(decoded == target);
    decoded.verify_annotated_tag(&bytes).unwrap();
    assert_eq!(observed.as_bytes(), sealed.as_bytes());
    let (_, recovered_name, discovered) =
        EncryptedRef::discover(&root, sealed.remote_key(), sealed.as_bytes()).unwrap();
    assert_eq!(recovered_name, logical);
    assert!(discovered == target);
    for invalid in ["HEAD", "refs/heads/release", "refs/remotes/old/release"] {
        assert!(EncryptedRef::seal(&root, invalid, &target).is_err());
    }
    let resealed = EncryptedRef::seal(&root, logical, &target).unwrap();
    assert_ne!(resealed.as_bytes(), observed.as_bytes());
    for offset in 0..sealed.as_bytes().len() {
        let mut changed = sealed.as_bytes().to_vec();
        changed[offset] ^= 1;
        assert!(EncryptedRef::open(&root, logical, &changed).is_err());
        assert!(EncryptedRef::open(&root, logical, &sealed.as_bytes()[..offset]).is_err());
    }
    let false_dependency = RefTarget::AnnotatedTag { object: *object, commit: [9; 32] };
    assert!(false_dependency.verify_annotated_tag(&bytes).is_err());
}

#[test]
fn encrypted_refs_bind_exact_ciphertext_identity_and_route() {
    let root = fixture_root();
    let commit = RefTarget::Commit([7; 32]);
    let sealed = EncryptedRef::seal(&root, "refs/heads/main", &commit).unwrap();
    let (observed, decoded) =
        EncryptedRef::open(&root, "refs/heads/main", sealed.as_bytes()).unwrap();
    assert!(decoded == commit);
    assert_eq!(observed.as_bytes(), sealed.as_bytes());
    assert_eq!(observed.remote_key(), sealed.remote_key());
    assert_eq!(
        &sealed.as_bytes()[13..45],
        root.route_commit(&[7; 32]).unwrap()
    );
    let resealed = EncryptedRef::seal(&root, "refs/heads/main", &commit).unwrap();
    assert_ne!(resealed.as_bytes(), observed.as_bytes());
    assert_eq!(resealed.remote_key(), observed.remote_key());
    assert!(EncryptedRef::open(&root, "refs/heads/other", sealed.as_bytes()).is_err());
    let other = RootKey::generate(RepositoryId::generate().unwrap()).unwrap();
    assert!(EncryptedRef::open(&other, "refs/heads/main", sealed.as_bytes()).is_err());
    for index in 0..sealed.as_bytes().len() {
        let mut corrupt = sealed.as_bytes().to_vec();
        corrupt[index] ^= 1;
        assert!(EncryptedRef::open(&root, "refs/heads/main", &corrupt).is_err());
        assert!(EncryptedRef::open(&root, "refs/heads/main", &sealed.as_bytes()[..index]).is_err());
    }
    let mut trailing = sealed.as_bytes().to_vec();
    trailing.push(0);
    assert!(EncryptedRef::open(&root, "refs/heads/main", &trailing).is_err());
    assert!(matches!(
        EncryptedRef::open(&root, "HEAD", &vec![0; 16385]),
        Err(EncryptionError::TooLarge)
    ));
}

#[test]
fn opaque_ref_discovery_authenticates_identity_class_repository_and_target() {
    let root = fixture_root();
    for (logical, target) in [
        ("refs/heads/private-name", RefTarget::Commit([7; 32])),
        (
            "HEAD",
            RefTarget::Symbolic("refs/heads/private-name".into()),
        ),
    ] {
        let sealed = EncryptedRef::seal(&root, logical, &target).unwrap();
        let (observed, discovered, recovered) =
            EncryptedRef::discover(&root, sealed.remote_key(), sealed.as_bytes()).unwrap();
        assert_eq!(discovered, logical);
        assert!(recovered == target);
        assert_eq!(observed.as_bytes(), sealed.as_bytes());
        let foreign = RootKey::generate(RepositoryId::generate().unwrap()).unwrap();
        assert!(EncryptedRef::discover(&foreign, sealed.remote_key(), sealed.as_bytes()).is_err());
        let other_key = format!("e2ee/refs/{}", "00".repeat(32));
        assert!(EncryptedRef::discover(&root, &other_key, sealed.as_bytes()).is_err());
        for invalid in [
            "refs/heads/private-name".to_owned(),
            "e2ee/refs/bad".to_owned(),
            format!("e2ee/indexes/{}", "00".repeat(32)),
        ] {
            assert!(EncryptedRef::discover(&root, &invalid, sealed.as_bytes()).is_err());
        }
    }
}

#[test]
fn symbolic_head_uses_opaque_branch_token_and_rejects_invalid_refs() {
    let root = fixture_root();
    let target = RefTarget::Symbolic("refs/heads/feature/中文".into());
    let head = EncryptedRef::seal(&root, "HEAD", &target).unwrap();
    let (_, decoded) = EncryptedRef::open(&root, "HEAD", head.as_bytes()).unwrap();
    assert!(decoded == target);
    let branch = EncryptedRef::seal(
        &root,
        "refs/heads/feature/中文",
        &RefTarget::Commit([1; 32]),
    )
    .unwrap();
    assert_eq!(
        hex(&head.as_bytes()[13..45]),
        branch.remote_key().rsplit('/').next().unwrap()
    );
    assert_eq!(head.as_bytes()[12], 2);
    assert!(EncryptedRef::seal(&root, "refs/heads/main", &target).is_err());
    assert!(
        EncryptedRef::seal(&root, "HEAD", &RefTarget::Symbolic("refs/tags/v1".into())).is_err()
    );
    for name in [
        "",
        "refs/heads/",
        "refs/heads/../secret",
        "refs/heads/a.lock",
        "refs/heads/a b",
        "refs/heads/a\\b",
    ] {
        assert!(EncryptedRef::seal(&root, name, &RefTarget::Commit([1; 32])).is_err());
    }
}

#[test]
fn authenticated_but_inconsistent_public_ref_route_is_rejected() {
    use chacha20poly1305::{
        XChaCha20Poly1305, XNonce,
        aead::{Aead, KeyInit, Payload},
    };
    let root = fixture_root();
    for (logical, target) in [
        ("refs/heads/main", RefTarget::Commit([7; 32])),
        (
            "refs/tags/release",
            RefTarget::AnnotatedTag { object: [9; 32], commit: [7; 32] },
        ),
    ] {
        let sealed = EncryptedRef::seal(&root, logical, &target).unwrap();
        let aad = |prefix: &[u8]| {
            codec::encode(&[
                b"graft/ref/v1",
                PROFILE,
                root.repository().as_bytes(),
                sealed.remote_key().as_bytes(),
                prefix,
            ])
            .unwrap()
        };
        let key = root.derive(b"ref").unwrap();
        let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref()).unwrap();
        let original = sealed.as_bytes();
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(
                    XNonce::from_slice(&original[45..69]),
                    Payload {
                        msg: &original[69..],
                        aad: &aad(&original[..69]),
                    },
                )
                .unwrap(),
        );
        let mut prefix = original[..69].to_vec();
        prefix[13] ^= 1;
        // A malicious key holder can authenticate an inconsistent prefix. Receivers
        // must still recompute its route from the hidden authoritative target.
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&prefix[45..69]),
                Payload { msg: &plaintext, aad: &aad(&prefix) },
            )
            .unwrap();
        prefix.extend_from_slice(&ciphertext);
        assert!(matches!(
            EncryptedRef::open(&root, logical, &prefix),
            Err(EncryptionError::AuthenticationFailed)
        ));
    }
}

pub(super) fn fixture_root() -> RootKey {
    let mut uuid = std::array::from_fn(|i| i as u8);
    uuid[6] = 0x46;
    uuid[8] = 0x88;
    RootKey::import(
        RepositoryId::from_bytes(uuid).unwrap(),
        Zeroizing::new(std::array::from_fn(|i| i as u8)),
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn derivation_matches_independent_python_hmac_hkdf_vector() {
    let root = fixture_root();
    assert_eq!(
        hex(root.derive(b"name").unwrap().as_ref()),
        "8c1af960ec5c40c091d8fb560226b8721b8b36890f94512738cbc5ddec1d3710"
    );
    assert_eq!(
        hex(&root.route_commit(&[7; 32]).unwrap()),
        "917df4927891ab722cdd925c17ba1b4b49354794873be3feaaab9c1ea75e5d04"
    );
    assert_ne!(
        root.derive(b"name").unwrap(),
        root.derive(b"route").unwrap()
    );
    let other = RootKey::import(
        RepositoryId::generate().unwrap(),
        Zeroizing::new(std::array::from_fn(|i| i as u8)),
    );
    assert_ne!(
        root.route_commit(&[7; 32]).unwrap(),
        other.route_commit(&[7; 32]).unwrap()
    );
    assert!(format!("{root:?}").contains("[REDACTED]"));
}

#[test]
fn codec_rejects_truncation_extra_fields_and_trailing_bytes() {
    let encoded = codec::encode(&[b"A", b"B"]).unwrap();
    assert_eq!(hex(&encoded), "000200000001410000000142");
    assert_eq!(
        codec::decode(&encoded, 2, 32).unwrap(),
        [b"A".as_slice(), b"B".as_slice()]
    );
    for end in 0..encoded.len() {
        assert!(codec::decode(&encoded[..end], 2, 32).is_err());
    }
    let mut trailing = encoded.clone();
    trailing.push(0);
    assert!(codec::decode(&trailing, 2, 32).is_err());
    assert!(codec::decode(&encoded, 1, 32).is_err());
    assert_eq!(
        codec::decode(&encoded, 2, 4),
        Err(EncryptionError::TooLarge)
    );
    assert!(codec::decode(&[0, 1, 255, 255, 255, 255], 1, 32).is_err());
}

#[test]
fn password_change_preserves_root_identity_and_rejects_rollback_and_forks() {
    let root = fixture_root();
    let original = EncryptedConfig::create(&root, b" initial password ").unwrap();
    assert_eq!(original.as_bytes().len(), 206);
    let unlocked = original
        .unlock(b" initial password ", Some(root.repository()))
        .unwrap();
    assert_eq!(
        root.route_commit(&[7; 32]).unwrap(),
        unlocked.route_commit(&[7; 32]).unwrap()
    );
    assert!(matches!(
        original.unlock(b"initial password", None),
        Err(EncryptionError::UnlockFailed)
    ));
    let checkpoint = original.checkpoint(&root).unwrap();
    let changed = original
        .password_change_candidate(&root, b"new password")
        .unwrap();
    assert_eq!(changed.generation(), 2);
    let new_root = changed
        .unlock(b"new password", Some(root.repository()))
        .unwrap();
    assert_eq!(
        root.route_commit(&[7; 32]).unwrap(),
        new_root.route_commit(&[7; 32]).unwrap()
    );
    assert!(matches!(
        changed.unlock(b" initial password ", None),
        Err(EncryptionError::UnlockFailed)
    ));
    let advanced = checkpoint.validate(&changed, &root).unwrap();
    assert_eq!(
        advanced.validate(&original, &root),
        Err(EncryptionError::ConfigRollbackDetected)
    );
    let competing = original
        .password_change_candidate(&root, b"concurrent password")
        .unwrap();
    assert_eq!(
        advanced.validate(&competing, &root),
        Err(EncryptionError::ConfigRollbackDetected)
    );
    // Old wrapping copies remain decryptable: changing passwords is not revocation.
    assert!(original.unlock(b" initial password ", None).is_ok());
}

#[test]
fn configuration_authenticates_every_byte_and_pins_identity() {
    let root = fixture_root();
    let config = EncryptedConfig::create(&root, b"test password").unwrap();
    for i in 0..config.as_bytes().len() {
        let mut mutated = config.as_bytes().to_vec();
        mutated[i] ^= 1;
        if let Ok(parsed) = EncryptedConfig::parse(&mutated) {
            assert!(
                parsed.authenticate(&root).is_err(),
                "byte {i} was not authenticated"
            );
        }
    }
    assert!(matches!(
        config.unlock(b"test password", Some(RepositoryId::generate().unwrap())),
        Err(EncryptionError::IdentityChanged)
    ));
    let mut damaged = config.as_bytes().to_vec();
    damaged[150] ^= 1;
    assert!(matches!(
        EncryptedConfig::parse(&damaged)
            .unwrap()
            .unlock(b"test password", None),
        Err(EncryptionError::UnlockFailed)
    ));
    let mut damaged_mac = config.as_bytes().to_vec();
    damaged_mac[200] ^= 1;
    assert!(matches!(
        EncryptedConfig::parse(&damaged_mac)
            .unwrap()
            .unlock(b"test password", None),
        Err(EncryptionError::UnlockFailed)
    ));
}

#[test]
fn unsafe_inputs_fail_before_kdf() {
    let root = fixture_root();
    for password in [Vec::new(), vec![b'a'; 1025], vec![0xff]] {
        assert!(matches!(
            EncryptedConfig::create(&root, &password),
            Err(EncryptionError::InvalidPassword)
        ));
    }
    assert!(EncryptedConfig::parse(&[0; 205]).is_err());
    assert!(EncryptedConfig::parse(&[0; 207]).is_err());
    let mut hostile = [0; 206];
    hostile[..8].copy_from_slice(b"GRAFTEC1");
    hostile[8..10].copy_from_slice(&1_u16.to_be_bytes());
    hostile[10..12].copy_from_slice(&1_u16.to_be_bytes());
    hostile[12..28].copy_from_slice(root.repository().as_bytes());
    hostile[28..36].copy_from_slice(&1_u64.to_be_bytes());
    hostile[68..70].copy_from_slice(&65535_u16.to_be_bytes());
    assert!(matches!(
        EncryptedConfig::parse(&hostile),
        Err(EncryptionError::UnsupportedProfile)
    ));
}

pub(super) fn encrypted_object(
    root: &RootKey,
    key: &str,
    plaintext: &[u8],
    size: ChunkSize,
) -> (Vec<u8>, Vec<Vec<u8>>, u64) {
    let mut writer = ObjectEncryptor::new(
        root,
        key,
        plaintext.len() as u64,
        Sha256::digest(plaintext).into(),
        size,
    )
    .unwrap();
    let header = writer.header_bytes().to_vec();
    let total = writer.ciphertext_length().unwrap();
    let mut chunks = Vec::new();
    if plaintext.is_empty() {
        chunks.push(writer.encrypt_next(&[]).unwrap());
    } else {
        for chunk in plaintext.chunks(writer.chunk_size()) {
            chunks.push(writer.encrypt_next(chunk).unwrap());
        }
    }
    writer.finish().unwrap();
    (header, chunks, total)
}

#[test]
fn opaque_object_discovery_authenticates_names_classes_and_repository_binding() {
    let root = fixture_root();
    for logical in [
        "objects/pack/test.pack",
        "objects/pack/test.idx",
        "objects/test",
        "store/files/test",
        "logs/test/commits/1",
        "segments/test",
    ] {
        let (header, _, total) = encrypted_object(&root, logical, b"content", ChunkSize::KiB64);
        let key = object::remote_key(&root, logical).unwrap();
        let reader = ObjectReader::discover(&root, &key, &header, total).unwrap();
        assert_eq!(reader.logical_key(), logical);
        assert_eq!(
            ObjectReader::header_length(&header[..72]).unwrap(),
            header.len()
        );
        let other = RootKey::generate(RepositoryId::generate().unwrap()).unwrap();
        assert!(ObjectReader::discover(&other, &key, &header, total).is_err());
        let wrong = format!("e2ee/segments/{}", "0".repeat(64));
        assert!(ObjectReader::discover(&root, &wrong, &header, total).is_err());
        for invalid in [
            logical.to_owned(),
            format!("{key}/extra"),
            key.to_uppercase(),
            format!("e2ee/refs/{}", "0".repeat(64)),
        ] {
            assert!(ObjectReader::discover(&root, &invalid, &header, total).is_err());
        }
    }
}

#[test]
fn objects_roundtrip_empty_boundaries_and_all_chunk_sizes() {
    let root = fixture_root();
    for (size, bytes) in [
        (ChunkSize::KiB16, 16384),
        (ChunkSize::KiB64, 65536),
        (ChunkSize::KiB256, 262144),
    ] {
        for length in [0, 1, bytes - 1, bytes, bytes + 1, 2 * bytes] {
            let plaintext: Vec<u8> = (0..length).map(|i| (i % 251) as u8).collect();
            let (header, chunks, total) =
                encrypted_object(&root, "segments/test", &plaintext, size);
            let reader = ObjectReader::open(&root, "segments/test", &header, total).unwrap();
            let mut restored = Vec::new();
            for (i, chunk) in chunks.iter().enumerate() {
                restored.extend(reader.decrypt_chunk(i as u64, chunk).unwrap());
            }
            assert_eq!(restored, plaintext);
            assert_eq!(reader.chunk_count(), chunks.len() as u64);
            assert!(reader.ciphertext_range(0..0).unwrap().is_empty());
            assert!(reader.ciphertext_range(0..(length as u64 + 1)).is_err());
        }
    }
}

#[test]
fn chunk_authentication_rejects_swaps_replacement_truncation_and_header_tampering() {
    let root = fixture_root();
    let plaintext = vec![42; 32769];
    let (header, chunks, total) =
        encrypted_object(&root, "segments/one", &plaintext, ChunkSize::KiB16);
    let reader = ObjectReader::open(&root, "segments/one", &header, total).unwrap();
    assert!(reader.decrypt_chunk(0, &chunks[1]).is_err());
    assert!(
        reader
            .decrypt_chunk(0, &chunks[0][..chunks[0].len() - 1])
            .is_err()
    );
    let mut corrupted = chunks[0].clone();
    corrupted[100] ^= 1;
    assert!(reader.decrypt_chunk(0, &corrupted).is_err());
    let (_, another_chunks, _) =
        encrypted_object(&root, "segments/one", &plaintext, ChunkSize::KiB16);
    assert!(reader.decrypt_chunk(0, &another_chunks[0]).is_err());
    assert!(ObjectReader::open(&root, "segments/two", &header, total).is_err());
    let other = RootKey::generate(RepositoryId::generate().unwrap()).unwrap();
    assert!(ObjectReader::open(&other, "segments/one", &header, total).is_err());
    assert!(ObjectReader::open(&root, "segments/one", &header, total - 1).is_err());
    for i in 0..header.len() {
        let mut corrupted = header.clone();
        corrupted[i] ^= 1;
        assert!(
            ObjectReader::open(&root, "segments/one", &corrupted, total).is_err(),
            "header byte {i}"
        );
    }
    let first = reader.ciphertext_range(0..1).unwrap();
    assert_eq!(first.end - first.start, 16384 + 16);
    let crossed = reader.ciphertext_range(16383..16385).unwrap();
    assert_eq!(crossed.end - crossed.start, 2 * (16384 + 16));
    let last = reader.ciphertext_range(32768..32769).unwrap();
    assert_eq!(last.end, total);
    assert_eq!(last.end - last.start, 17);
}

#[test]
fn encryption_refuses_nonce_reuse_incomplete_or_changed_snapshots() {
    let root = fixture_root();
    let digest = Sha256::digest(b"abcd").into();
    let mut writer =
        ObjectEncryptor::new(&root, "objects/one", 4, digest, ChunkSize::KiB16).unwrap();
    assert!(writer.encrypt_next(b"abc").is_err());
    writer.encrypt_next(b"abcd").unwrap();
    assert!(writer.encrypt_next(b"abcd").is_err());
    writer.finish().unwrap();
    let mut changed =
        ObjectEncryptor::new(&root, "objects/one", 4, digest, ChunkSize::KiB16).unwrap();
    changed.encrypt_next(b"abce").unwrap();
    assert_eq!(changed.finish(), Err(EncryptionError::SnapshotMismatch));
    let incomplete =
        ObjectEncryptor::new(&root, "objects/one", 4, digest, ChunkSize::KiB16).unwrap();
    assert_eq!(incomplete.finish(), Err(EncryptionError::InvalidChunk));
    for logical in [
        "HEAD",
        "refs/heads/main",
        "segments/../one",
        "/segments/one",
        "segments//one",
        "unknown/one",
    ] {
        assert!(ObjectEncryptor::new(&root, logical, 4, digest, ChunkSize::KiB16).is_err());
    }
    assert!(
        ObjectEncryptor::new(&root, "objects/one", u64::MAX, digest, ChunkSize::KiB16).is_err()
    );
}
