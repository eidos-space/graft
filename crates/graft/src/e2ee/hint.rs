use super::{
    EncryptionError, PROFILE, RepositoryId, Result, RootKey, SUITE, codec, mac, verify_mac,
};

pub(crate) const MAX_ENTRIES: usize = 4096;
pub(crate) const MAX_PARENTS: usize = 32;
pub(crate) const MAX_LENGTH: usize = 1024 * 1024;
const PREFIX_LENGTH: usize = 28;

/// Fixed frame, key fields and entry-list framing for an encrypted pack hint.
pub(crate) const EMPTY_HINT_LENGTH: usize = PREFIX_LENGTH + 32 + 2 + 4 + 75 + 4 + 77 + 4 + 2;

/// Length-prefix framing plus one route token and the ordered parent tokens.
pub(crate) fn entry_length(parents: usize) -> Option<usize> {
    parents.checked_mul(36)?.checked_add(48)
}

/// Authoritative IDs from verified commit objects. Never upload this structure.
#[derive(Clone)]
pub struct CommitTopology {
    pub commit: [u8; 32],
    pub parents: Vec<[u8; 32]>,
}

#[derive(PartialEq, Eq)]
struct RoutedCommit {
    commit: [u8; 32],
    parents: Vec<[u8; 32]>,
}

/// Client-authenticated, deliberately public topology for one immutable pack.
/// Authentication alone does not prove the topology matches the actual pack.
pub struct RoutingHint {
    repository: RepositoryId,
    key: String,
    bytes: Vec<u8>,
    commits: Vec<RoutedCommit>,
    pack: String,
    index: String,
}

impl RoutingHint {
    pub fn create(
        root: &RootKey,
        pack_key: &str,
        index_key: &str,
        commits: &[CommitTopology],
    ) -> Result<Self> {
        let key = hint_key(pack_key, index_key)?;
        let commits = routed(root, commits)?;
        let entries = encode_entries(&commits)?;
        let body = codec::encode(&[pack_key.as_bytes(), index_key.as_bytes(), &entries])?;
        let mut bytes = Vec::with_capacity(PREFIX_LENGTH + body.len() + 32);
        bytes.extend_from_slice(b"GRAFTEH1");
        bytes.extend_from_slice(&1_u16.to_be_bytes());
        bytes.extend_from_slice(&SUITE.to_be_bytes());
        bytes.extend_from_slice(root.repository().as_bytes());
        let input = mac_input(root, &key, &bytes, &body)?;
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(&mac(root.derive(b"hint")?.as_ref(), &input));
        if bytes.len() > MAX_LENGTH {
            return Err(EncryptionError::TooLarge);
        }
        Ok(Self {
            repository: root.repository(),
            key,
            bytes,
            commits,
            pack: pack_key.to_owned(),
            index: index_key.to_owned(),
        })
    }

    /// Expected keys must come from the client's verified pack/index identity.
    pub fn open(root: &RootKey, pack_key: &str, index_key: &str, bytes: &[u8]) -> Result<Self> {
        let key = hint_key(pack_key, index_key)?;
        let (prefix, body, tag) = split_frame(root, bytes)?;
        let input = mac_input(root, &key, prefix, body)?;
        verify_mac(root.derive(b"hint")?.as_ref(), &input, tag)?;
        let fields = codec::decode(body, 3, MAX_LENGTH)?;
        if fields[0] != pack_key.as_bytes() || fields[1] != index_key.as_bytes() {
            return Err(EncryptionError::AuthenticationFailed);
        }
        let entries = codec::decode_bounded(fields[2], MAX_ENTRIES, MAX_LENGTH)?;
        let mut commits = Vec::with_capacity(entries.len());
        for entry in entries {
            let fields = codec::decode(entry, 2, MAX_LENGTH)?;
            let commit = id(fields[0])?;
            let parents = codec::decode_bounded(fields[1], MAX_PARENTS, MAX_LENGTH)?
                .into_iter()
                .map(id)
                .collect::<Result<Vec<_>>>()?;
            validate_parents(&parents)?;
            if commits
                .last()
                .is_some_and(|last: &RoutedCommit| last.commit >= commit)
            {
                return Err(EncryptionError::InvalidEncoding);
            }
            commits.push(RoutedCommit { commit, parents });
        }
        Ok(Self {
            repository: root.repository(),
            key,
            bytes: bytes.to_vec(),
            commits,
            pack: pack_key.to_owned(),
            index: index_key.to_owned(),
        })
    }

    /// Authenticate a requested public hint before discovering its associated keys.
    /// Pack topology still requires verification against the actual decrypted pack.
    pub fn discover(root: &RootKey, key: &str, bytes: &[u8]) -> Result<Self> {
        validate_remote_key(key)?;
        let (prefix, body, tag) = split_frame(root, bytes)?;
        verify_mac(
            root.derive(b"hint")?.as_ref(),
            &mac_input(root, key, prefix, body)?,
            tag,
        )?;
        let fields = codec::decode(body, 3, MAX_LENGTH)?;
        let pack = std::str::from_utf8(fields[0]).map_err(|_| EncryptionError::InvalidEncoding)?;
        let index = std::str::from_utf8(fields[1]).map_err(|_| EncryptionError::InvalidEncoding)?;
        let hint = Self::open(root, pack, index, bytes)?;
        if hint.key != key {
            return Err(EncryptionError::AuthenticationFailed);
        }
        Ok(hint)
    }

    pub fn pack_key(&self) -> &str {
        &self.pack
    }

    pub fn index_key(&self) -> &str {
        &self.index
    }

    /// Require a complete match to all verified commits in the associated pack.
    /// Callers still own authoritative object hashing and graph completeness.
    pub fn verify_topology(&self, root: &RootKey, actual: &[CommitTopology]) -> Result<()> {
        if root.repository() != self.repository {
            return Err(EncryptionError::IdentityChanged);
        }
        if self.commits != routed(root, actual)? {
            return Err(EncryptionError::AuthenticationFailed);
        }
        Ok(())
    }

    pub fn remote_key(&self) -> &str {
        &self.key
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

pub(crate) fn validate_remote_key(key: &str) -> Result<()> {
    opaque_token(key, "e2ee/hints/")?;
    Ok(())
}

fn routed(root: &RootKey, actual: &[CommitTopology]) -> Result<Vec<RoutedCommit>> {
    if actual.len() > MAX_ENTRIES {
        return Err(EncryptionError::TooLarge);
    }
    let mut routed = Vec::with_capacity(actual.len());
    for entry in actual {
        if entry.parents.len() > MAX_PARENTS {
            return Err(EncryptionError::TooLarge);
        }
        validate_parents(&entry.parents)?;
        let parents = entry
            .parents
            .iter()
            .map(|parent| root.route_commit(parent))
            .collect::<Result<Vec<_>>>()?;
        routed.push(RoutedCommit {
            commit: root.route_commit(&entry.commit)?,
            parents,
        });
    }
    routed.sort_unstable_by_key(|entry| entry.commit);
    if routed
        .windows(2)
        .any(|pair| pair[0].commit == pair[1].commit)
    {
        return Err(EncryptionError::InvalidEncoding);
    }
    Ok(routed)
}

fn encode_entries(commits: &[RoutedCommit]) -> Result<Vec<u8>> {
    let mut entries = Vec::with_capacity(commits.len());
    for entry in commits {
        let parents = entry
            .parents
            .iter()
            .map(|parent| parent.as_slice())
            .collect::<Vec<_>>();
        let parents = codec::encode(&parents)?;
        entries.push(codec::encode(&[&entry.commit, &parents])?);
    }
    codec::encode(&entries.iter().map(Vec::as_slice).collect::<Vec<_>>())
}

fn split_frame<'a>(root: &RootKey, bytes: &'a [u8]) -> Result<(&'a [u8], &'a [u8], &'a [u8])> {
    if bytes.len() > MAX_LENGTH {
        return Err(EncryptionError::TooLarge);
    }
    if bytes.len() < PREFIX_LENGTH + 32 {
        return Err(EncryptionError::InvalidEncoding);
    }
    let mut prefix = codec::Cursor::new(&bytes[..PREFIX_LENGTH]);
    if prefix.take(8)? != b"GRAFTEH1" {
        return Err(EncryptionError::InvalidEncoding);
    }
    if u16::from_be_bytes(prefix.array()?) != 1 || u16::from_be_bytes(prefix.array()?) != SUITE {
        return Err(EncryptionError::UnsupportedProfile);
    }
    if prefix.take(16)? != root.repository().as_bytes() {
        return Err(EncryptionError::IdentityChanged);
    }
    prefix.finish()?;
    Ok((
        &bytes[..PREFIX_LENGTH],
        &bytes[PREFIX_LENGTH..bytes.len() - 32],
        &bytes[bytes.len() - 32..],
    ))
}

fn hint_key(pack: &str, index: &str) -> Result<String> {
    let token = opaque_token(pack, "e2ee/packs/")?;
    opaque_token(index, "e2ee/indexes/")?;
    Ok(format!("e2ee/hints/{token}"))
}

fn opaque_token<'a>(key: &'a str, prefix: &str) -> Result<&'a str> {
    let token = key
        .strip_prefix(prefix)
        .ok_or(EncryptionError::InvalidObjectKey)?;
    if token.len() != 64
        || !token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(EncryptionError::InvalidObjectKey);
    }
    Ok(token)
}

fn validate_parents(parents: &[[u8; 32]]) -> Result<()> {
    for (index, parent) in parents.iter().enumerate() {
        if parents[..index].contains(parent) {
            return Err(EncryptionError::InvalidEncoding);
        }
    }
    Ok(())
}

fn id(bytes: &[u8]) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| EncryptionError::InvalidEncoding)
}

fn mac_input(root: &RootKey, key: &str, prefix: &[u8], body: &[u8]) -> Result<Vec<u8>> {
    codec::encode(&[
        b"graft/routing-hint/v1",
        PROFILE,
        root.repository().as_bytes(),
        key.as_bytes(),
        prefix,
        body,
    ])
}
