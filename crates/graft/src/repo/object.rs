use std::{
    fmt::{self, Display},
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::core::{
    LogId, VolumeId, commit_hash::CommitHash, lsn::LSN, lsn::LSNRangeExt, page_count::PageCount,
};

pub const OBJECT_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum ObjectErr {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid object id `{0}`")]
    InvalidObjectId(String),

    #[error("invalid object header")]
    InvalidHeader,

    #[error("unsupported object format version {0}")]
    UnsupportedVersion(u32),

    #[error("object payload length mismatch: expected {expected}, got {actual}")]
    PayloadLengthMismatch { expected: usize, actual: usize },

    #[error("invalid {kind} object: {message}")]
    InvalidObject { kind: &'static str, message: String },

    #[error("object id mismatch: expected {expected}, got {actual}")]
    ObjectIdMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
}

pub type Result<T> = std::result::Result<T, ObjectErr>;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ObjectId(String);

impl ObjectId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_object_id(&value)?;
        Ok(Self(value))
    }

    pub fn for_bytes(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn short(&self) -> &str {
        &self.0[..12]
    }
}

impl Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ObjectId {
    type Err = ObjectErr;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl ObjectKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Commit => "commit",
            Self::Tag => "tag",
        }
    }
}

impl Display for ObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ObjectKind {
    type Err = ObjectErr;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "blob" => Ok(Self::Blob),
            "tree" => Ok(Self::Tree),
            "commit" => Ok(Self::Commit),
            "tag" => Ok(Self::Tag),
            _ => Err(ObjectErr::InvalidHeader),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Object {
    Blob(BlobObject),
    Tree(TreeObject),
    Commit(CommitObject),
    Tag(TagObject),
}

impl Object {
    pub fn kind(&self) -> ObjectKind {
        match self {
            Self::Blob(_) => ObjectKind::Blob,
            Self::Tree(_) => ObjectKind::Tree,
            Self::Commit(_) => ObjectKind::Commit,
            Self::Tag(_) => ObjectKind::Tag,
        }
    }

    pub fn id(&self) -> ObjectId {
        ObjectId::for_bytes(&self.canonical_bytes())
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        let payload = self.canonical_payload();
        let header = format!(
            "graft-object {OBJECT_FORMAT_VERSION} {} {}\0",
            self.kind(),
            payload.len()
        );
        let mut bytes = header.into_bytes();
        bytes.extend_from_slice(&payload);
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let Some(nul) = bytes.iter().position(|byte| *byte == 0) else {
            return Err(ObjectErr::InvalidHeader);
        };
        let header = std::str::from_utf8(&bytes[..nul])
            .map_err(|err| ObjectErr::InvalidObject { kind: "header", message: err.to_string() })?;
        let mut parts = header.split(' ');
        if parts.next() != Some("graft-object") {
            return Err(ObjectErr::InvalidHeader);
        }
        let version: u32 = parts
            .next()
            .ok_or(ObjectErr::InvalidHeader)?
            .parse()
            .map_err(|_| ObjectErr::InvalidHeader)?;
        if version != OBJECT_FORMAT_VERSION {
            return Err(ObjectErr::UnsupportedVersion(version));
        }
        let kind: ObjectKind = parts.next().ok_or(ObjectErr::InvalidHeader)?.parse()?;
        let expected: usize = parts
            .next()
            .ok_or(ObjectErr::InvalidHeader)?
            .parse()
            .map_err(|_| ObjectErr::InvalidHeader)?;
        if parts.next().is_some() {
            return Err(ObjectErr::InvalidHeader);
        }

        let payload = &bytes[nul + 1..];
        if payload.len() != expected {
            return Err(ObjectErr::PayloadLengthMismatch { expected, actual: payload.len() });
        }
        let payload = std::str::from_utf8(payload).map_err(|err| ObjectErr::InvalidObject {
            kind: kind.as_str(),
            message: err.to_string(),
        })?;

        match kind {
            ObjectKind::Blob => Ok(Self::Blob(BlobObject::decode(payload)?)),
            ObjectKind::Tree => Ok(Self::Tree(TreeObject::decode(payload)?)),
            ObjectKind::Commit => Ok(Self::Commit(CommitObject::decode(payload)?)),
            ObjectKind::Tag => Ok(Self::Tag(TagObject::decode(payload)?)),
        }
    }

    fn canonical_payload(&self) -> Vec<u8> {
        match self {
            Self::Blob(blob) => blob.canonical_payload(),
            Self::Tree(tree) => tree.canonical_payload(),
            Self::Commit(commit) => commit.canonical_payload(),
            Self::Tag(tag) => tag.canonical_payload(),
        }
        .into_bytes()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobObject {
    SqliteSnapshot(SqliteSnapshotBlob),
    File(FileBlob),
    LargeFilePointer(LargeFilePointerBlob),
}

impl BlobObject {
    fn canonical_payload(&self) -> String {
        match self {
            Self::SqliteSnapshot(blob) => blob.canonical_payload(),
            Self::File(blob) => blob.canonical_payload(),
            Self::LargeFilePointer(blob) => blob.canonical_payload(),
        }
    }

    fn decode(payload: &str) -> Result<Self> {
        let mut lines = payload.lines();
        match lines.next() {
            Some("sqlite-snapshot-v1") => {
                SqliteSnapshotBlob::decode(lines).map(Self::SqliteSnapshot)
            }
            Some("file-blob-v1") => {
                FileBlob::decode(lines, FileBlobEncoding::Base58).map(Self::File)
            }
            Some("file-blob-v2") => {
                FileBlob::decode(lines, FileBlobEncoding::Base64).map(Self::File)
            }
            Some("large-file-pointer-v1") => {
                LargeFilePointerBlob::decode(lines).map(Self::LargeFilePointer)
            }
            _ => Err(ObjectErr::InvalidObject {
                kind: "blob",
                message: "missing supported blob header".to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteSnapshotBlob {
    pub volume: VolumeId,
    pub page_count: PageCount,
    pub ranges: Vec<SqliteSnapshotRange>,
}

impl SqliteSnapshotBlob {
    fn canonical_payload(&self) -> String {
        let mut out = format!(
            "sqlite-snapshot-v1\nvolume {}\npage_count {}\n",
            self.volume, self.page_count
        );
        for range in &self.ranges {
            out.push_str(&format!(
                "range {} {} {}\n",
                range.log, range.start, range.end
            ));
            for commit in &range.commits {
                out.push_str(&format!("commit {} {}\n", commit.lsn, commit.commit_hash));
            }
        }
        out
    }

    fn decode<'a>(lines: impl Iterator<Item = &'a str>) -> Result<Self> {
        let mut volume = None;
        let mut page_count = None;
        let mut ranges = Vec::new();
        let mut current_range: Option<SqliteSnapshotRange> = None;

        for line in lines {
            let mut parts = line.split(' ');
            match parts.next() {
                Some("volume") => {
                    volume = Some(parse_field(parts.next(), "blob", "volume")?);
                    ensure_no_extra(parts, "blob")?;
                }
                Some("page_count") => {
                    let raw: u32 = parse_field(parts.next(), "blob", "page_count")?;
                    page_count = Some(PageCount::new(raw));
                    ensure_no_extra(parts, "blob")?;
                }
                Some("range") => {
                    if let Some(range) = current_range.take() {
                        ranges.push(range);
                    }
                    let log = parse_field(parts.next(), "blob", "range log")?;
                    let start = parse_field(parts.next(), "blob", "range start")?;
                    let end = parse_field(parts.next(), "blob", "range end")?;
                    ensure_no_extra(parts, "blob")?;
                    current_range =
                        Some(SqliteSnapshotRange { log, start, end, commits: Vec::new() });
                }
                Some("commit") => {
                    let Some(range) = current_range.as_mut() else {
                        return Err(ObjectErr::InvalidObject {
                            kind: "blob",
                            message: "commit entry before range".to_string(),
                        });
                    };
                    let lsn = parse_field(parts.next(), "blob", "commit lsn")?;
                    let value = parse_field::<CommitHash>(parts.next(), "blob", "commit hash")?;
                    ensure_no_extra(parts, "blob")?;
                    range
                        .commits
                        .push(SqliteSnapshotCommit { lsn, commit_hash: value });
                }
                Some("") => {}
                _ => {
                    return Err(ObjectErr::InvalidObject {
                        kind: "blob",
                        message: format!("invalid line `{line}`"),
                    });
                }
            }
        }
        if let Some(range) = current_range.take() {
            ranges.push(range);
        }
        validate_sqlite_snapshot_ranges(&ranges)?;

        Ok(Self {
            volume: volume.ok_or_else(|| missing("blob", "volume"))?,
            page_count: page_count.ok_or_else(|| missing("blob", "page_count"))?,
            ranges,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteSnapshotRange {
    pub log: LogId,
    pub start: LSN,
    pub end: LSN,
    pub commits: Vec<SqliteSnapshotCommit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteSnapshotCommit {
    pub lsn: LSN,
    pub commit_hash: CommitHash,
}

fn validate_sqlite_snapshot_ranges(ranges: &[SqliteSnapshotRange]) -> Result<()> {
    for range in ranges {
        let expected_count = (range.start..=range.end).len();
        if expected_count == 0 {
            return Err(ObjectErr::InvalidObject {
                kind: "blob",
                message: format!(
                    "range {:?} {}..={} is empty",
                    range.log, range.start, range.end
                ),
            });
        }
        if range.commits.len() as u64 != expected_count {
            return Err(ObjectErr::InvalidObject {
                kind: "blob",
                message: format!(
                    "range {:?} {}..={} has {} storage commit hashes; expected {}",
                    range.log,
                    range.start,
                    range.end,
                    range.commits.len(),
                    expected_count
                ),
            });
        }
        for (commit, expected_lsn) in range.commits.iter().zip((range.start..=range.end).iter()) {
            if commit.lsn != expected_lsn {
                return Err(ObjectErr::InvalidObject {
                    kind: "blob",
                    message: format!(
                        "range {:?} {}..={} has storage commit hash for LSN {}; expected {}",
                        range.log, range.start, range.end, commit.lsn, expected_lsn
                    ),
                });
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBlob {
    pub kind: FileContentKind,
    pub bytes: Vec<u8>,
}

impl FileBlob {
    fn canonical_payload(&self) -> String {
        format!(
            "file-blob-v2\nkind {}\nsize {}\nencoding base64\ndata {}\n",
            self.kind,
            self.bytes.len(),
            BASE64_STANDARD.encode(&self.bytes)
        )
    }

    fn decode<'a>(
        lines: impl Iterator<Item = &'a str>,
        encoding: FileBlobEncoding,
    ) -> Result<Self> {
        let mut kind = None;
        let mut size = None;
        let mut data = None;
        let mut declared_encoding = None;

        for line in lines {
            let mut parts = line.splitn(2, ' ');
            match parts.next() {
                Some("kind") => {
                    kind = Some(parse_field(parts.next(), "blob", "kind")?);
                }
                Some("size") => {
                    size = Some(parse_field(parts.next(), "blob", "size")?);
                }
                Some("encoding") => {
                    declared_encoding = Some(
                        parts
                            .next()
                            .ok_or_else(|| missing("blob", "encoding"))?
                            .to_string(),
                    );
                }
                Some("data") => {
                    let raw = parts.next().ok_or_else(|| missing("blob", "data"))?;
                    let bytes = match encoding {
                        FileBlobEncoding::Base58 => {
                            bs58::decode(raw).into_vec().map_err(|err| {
                                ObjectErr::InvalidObject {
                                    kind: "blob",
                                    message: format!("invalid file data encoding: {err}"),
                                }
                            })?
                        }
                        FileBlobEncoding::Base64 => {
                            BASE64_STANDARD
                                .decode(raw)
                                .map_err(|err| ObjectErr::InvalidObject {
                                    kind: "blob",
                                    message: format!("invalid file data encoding: {err}"),
                                })?
                        }
                    };
                    data = Some(bytes);
                }
                Some("") => {}
                _ => {
                    return Err(ObjectErr::InvalidObject {
                        kind: "blob",
                        message: format!("invalid line `{line}`"),
                    });
                }
            }
        }

        match encoding {
            FileBlobEncoding::Base58 if declared_encoding.is_some() => {
                return Err(ObjectErr::InvalidObject {
                    kind: "blob",
                    message: "file-blob-v1 must not declare an encoding".to_string(),
                });
            }
            FileBlobEncoding::Base64 if declared_encoding.as_deref() != Some("base64") => {
                return Err(ObjectErr::InvalidObject {
                    kind: "blob",
                    message: "file-blob-v2 requires `encoding base64`".to_string(),
                });
            }
            _ => {}
        }

        let bytes = data.ok_or_else(|| missing("blob", "data"))?;
        let size: usize = size.ok_or_else(|| missing("blob", "size"))?;
        if bytes.len() != size {
            return Err(ObjectErr::InvalidObject {
                kind: "blob",
                message: format!(
                    "file blob size mismatch: expected {size}, got {}",
                    bytes.len()
                ),
            });
        }

        Ok(Self {
            kind: kind.ok_or_else(|| missing("blob", "kind"))?,
            bytes,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileBlobEncoding {
    Base58,
    Base64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileContentKind {
    TextFile,
    BinaryFile,
}

impl Display for FileContentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TextFile => f.write_str("text_file"),
            Self::BinaryFile => f.write_str("binary_file"),
        }
    }
}

impl FromStr for FileContentKind {
    type Err = ObjectErr;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "text_file" => Ok(Self::TextFile),
            "binary_file" => Ok(Self::BinaryFile),
            _ => Err(ObjectErr::InvalidObject {
                kind: "blob",
                message: format!("invalid file content kind `{value}`"),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargeFilePointerBlob {
    pub kind: FileContentKind,
    pub content_hash: ObjectId,
    pub size: u64,
}

impl LargeFilePointerBlob {
    fn canonical_payload(&self) -> String {
        format!(
            "large-file-pointer-v1\nkind {}\nhash {}\nsize {}\n",
            self.kind, self.content_hash, self.size
        )
    }

    fn decode<'a>(lines: impl Iterator<Item = &'a str>) -> Result<Self> {
        let mut kind = None;
        let mut content_hash = None;
        let mut size = None;

        for line in lines {
            let mut parts = line.split(' ');
            match parts.next() {
                Some("kind") => {
                    kind = Some(parse_field(parts.next(), "blob", "kind")?);
                    ensure_no_extra(parts, "blob")?;
                }
                Some("hash") => {
                    content_hash = Some(parse_field(parts.next(), "blob", "hash")?);
                    ensure_no_extra(parts, "blob")?;
                }
                Some("size") => {
                    size = Some(parse_field(parts.next(), "blob", "size")?);
                    ensure_no_extra(parts, "blob")?;
                }
                Some("") => {}
                _ => {
                    return Err(ObjectErr::InvalidObject {
                        kind: "blob",
                        message: format!("invalid line `{line}`"),
                    });
                }
            }
        }

        Ok(Self {
            kind: kind.ok_or_else(|| missing("blob", "kind"))?,
            content_hash: content_hash.ok_or_else(|| missing("blob", "hash"))?,
            size: size.ok_or_else(|| missing("blob", "size"))?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeObject {
    pub entries: Vec<TreeEntry>,
}

impl TreeObject {
    pub fn new(mut entries: Vec<TreeEntry>) -> Result<Self> {
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        for entry in &entries {
            validate_path(&entry.path)?;
        }
        for pair in entries.windows(2) {
            if pair[0].path == pair[1].path {
                return Err(ObjectErr::InvalidObject {
                    kind: "tree",
                    message: format!("duplicate path `{}`", pair[0].path),
                });
            }
        }
        Ok(Self { entries })
    }

    fn canonical_payload(&self) -> String {
        let mut out = "tree-v1\n".to_string();
        for entry in &self.entries {
            out.push_str(&format!("{} {} {}\n", entry.mode, entry.oid, entry.path));
        }
        out
    }

    fn decode(payload: &str) -> Result<Self> {
        let mut lines = payload.lines();
        if lines.next() != Some("tree-v1") {
            return Err(ObjectErr::InvalidObject {
                kind: "tree",
                message: "missing tree-v1 header".to_string(),
            });
        }

        let mut entries = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let mut parts = line.splitn(3, ' ');
            let mode = parse_field(parts.next(), "tree", "mode")?;
            let oid = parse_field(parts.next(), "tree", "oid")?;
            let path = parts
                .next()
                .ok_or_else(|| missing("tree", "path"))?
                .to_string();
            entries.push(TreeEntry { mode, oid, path });
        }
        Self::new(entries)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: TreeEntryMode,
    pub oid: ObjectId,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeEntryMode {
    Regular,
    SqliteDatabase,
}

impl Display for TreeEntryMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Regular => f.write_str("100644"),
            Self::SqliteDatabase => f.write_str("160000"),
        }
    }
}

impl FromStr for TreeEntryMode {
    type Err = ObjectErr;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "100644" => Ok(Self::Regular),
            "160000" => Ok(Self::SqliteDatabase),
            _ => Err(ObjectErr::InvalidObject {
                kind: "tree",
                message: format!("invalid mode `{value}`"),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitObject {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: Signature,
    pub committer: Signature,
    pub repo_format_version: u32,
    pub tables: Vec<CommitTableSummary>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitTableSummary {
    pub name: String,
    pub inserts: usize,
    pub deletes: usize,
    pub updates: usize,
}

impl CommitObject {
    fn canonical_payload(&self) -> String {
        let mut out = format!("tree {}\n", self.tree);
        for parent in &self.parents {
            out.push_str(&format!("parent {parent}\n"));
        }
        out.push_str(&format!(
            "author-name {}\nauthor-email {}\nauthor-time {}\nauthor-tz {}\n",
            self.author.name, self.author.email, self.author.timestamp_ms, self.author.tz
        ));
        out.push_str(&format!(
            "committer-name {}\ncommitter-email {}\ncommitter-time {}\ncommitter-tz {}\n",
            self.committer.name,
            self.committer.email,
            self.committer.timestamp_ms,
            self.committer.tz
        ));
        out.push_str(&format!("graft-version {}\n", self.repo_format_version));
        for table in &self.tables {
            out.push_str(&format!(
                "table {} {} {} {}\n",
                encode_commit_table_name(&table.name),
                table.inserts,
                table.deletes,
                table.updates
            ));
        }
        out.push_str(&format!("\n{}", self.message));
        out
    }

    fn decode(payload: &str) -> Result<Self> {
        let (headers, message) =
            payload
                .split_once("\n\n")
                .ok_or_else(|| ObjectErr::InvalidObject {
                    kind: "commit",
                    message: "missing message separator".to_string(),
                })?;
        let mut tree = None;
        let mut parents = Vec::new();
        let mut author = SignatureBuilder::default();
        let mut committer = SignatureBuilder::default();
        let mut repo_format_version = None;
        let mut tables = Vec::new();

        for line in headers.lines() {
            let Some((key, value)) = line.split_once(' ') else {
                return Err(ObjectErr::InvalidObject {
                    kind: "commit",
                    message: format!("invalid header `{line}`"),
                });
            };
            match key {
                "tree" => tree = Some(value.parse()?),
                "parent" => parents.push(value.parse()?),
                "author-name" => author.name = Some(value.to_string()),
                "author-email" => author.email = Some(value.to_string()),
                "author-time" => author.timestamp_ms = Some(parse_value(value, "commit")?),
                "author-tz" => author.tz = Some(value.to_string()),
                "committer-name" => committer.name = Some(value.to_string()),
                "committer-email" => committer.email = Some(value.to_string()),
                "committer-time" => committer.timestamp_ms = Some(parse_value(value, "commit")?),
                "committer-tz" => committer.tz = Some(value.to_string()),
                "graft-version" => repo_format_version = Some(parse_value(value, "commit")?),
                "table" => tables.push(parse_commit_table_summary(value)?),
                _ => {
                    return Err(ObjectErr::InvalidObject {
                        kind: "commit",
                        message: format!("unknown header `{key}`"),
                    });
                }
            }
        }

        Ok(Self {
            tree: tree.ok_or_else(|| missing("commit", "tree"))?,
            parents,
            author: author.build("author")?,
            committer: committer.build("committer")?,
            repo_format_version: repo_format_version
                .ok_or_else(|| missing("commit", "graft-version"))?,
            tables,
            message: message.to_string(),
        })
    }
}

fn encode_commit_table_name(name: &str) -> String {
    bs58::encode(name.as_bytes()).into_string()
}

fn parse_commit_table_summary(value: &str) -> Result<CommitTableSummary> {
    let mut parts = value.split(' ');
    let raw_name = parts
        .next()
        .ok_or_else(|| missing("commit", "table name"))?;
    let inserts = parse_field(parts.next(), "commit", "table inserts")?;
    let deletes = parse_field(parts.next(), "commit", "table deletes")?;
    let updates = parse_field(parts.next(), "commit", "table updates")?;
    ensure_no_extra(parts, "commit")?;
    let name_bytes = bs58::decode(raw_name)
        .into_vec()
        .map_err(|err| ObjectErr::InvalidObject {
            kind: "commit",
            message: format!("invalid table name encoding: {err}"),
        })?;
    let name = String::from_utf8(name_bytes).map_err(|err| ObjectErr::InvalidObject {
        kind: "commit",
        message: format!("invalid table name: {err}"),
    })?;
    Ok(CommitTableSummary { name, inserts, deletes, updates })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub timestamp_ms: u64,
    pub tz: String,
}

impl Signature {
    pub fn new(
        name: impl Into<String>,
        email: impl Into<String>,
        timestamp_ms: u64,
        tz: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            email: email.into(),
            timestamp_ms,
            tz: tz.into(),
        }
    }
}

#[derive(Default)]
struct SignatureBuilder {
    name: Option<String>,
    email: Option<String>,
    timestamp_ms: Option<u64>,
    tz: Option<String>,
}

impl SignatureBuilder {
    fn build(self, prefix: &'static str) -> Result<Signature> {
        Ok(Signature {
            name: self
                .name
                .ok_or_else(|| missing("commit", &format!("{prefix}-name")))?,
            email: self
                .email
                .ok_or_else(|| missing("commit", &format!("{prefix}-email")))?,
            timestamp_ms: self
                .timestamp_ms
                .ok_or_else(|| missing("commit", &format!("{prefix}-time")))?,
            tz: self
                .tz
                .ok_or_else(|| missing("commit", &format!("{prefix}-tz")))?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagObject {
    pub object: ObjectId,
    pub object_type: ObjectKind,
    pub name: String,
    pub tagger: Signature,
    pub message: String,
}

impl TagObject {
    fn canonical_payload(&self) -> String {
        format!(
            "object {}\ntype {}\ntag {}\ntagger-name {}\ntagger-email {}\ntagger-time {}\ntagger-tz {}\n\n{}",
            self.object,
            self.object_type,
            self.name,
            self.tagger.name,
            self.tagger.email,
            self.tagger.timestamp_ms,
            self.tagger.tz,
            self.message
        )
    }

    fn decode(payload: &str) -> Result<Self> {
        let (headers, message) = payload
            .split_once("\n\n")
            .ok_or_else(|| missing("tag", "message separator"))?;
        let mut object = None;
        let mut object_type = None;
        let mut name = None;
        let mut tagger = SignatureBuilder::default();

        for line in headers.lines() {
            let Some((key, value)) = line.split_once(' ') else {
                return Err(ObjectErr::InvalidObject {
                    kind: "tag",
                    message: format!("invalid header `{line}`"),
                });
            };
            match key {
                "object" => object = Some(value.parse()?),
                "type" => object_type = Some(value.parse()?),
                "tag" => name = Some(value.to_string()),
                "tagger-name" => tagger.name = Some(value.to_string()),
                "tagger-email" => tagger.email = Some(value.to_string()),
                "tagger-time" => tagger.timestamp_ms = Some(parse_value(value, "tag")?),
                "tagger-tz" => tagger.tz = Some(value.to_string()),
                _ => {
                    return Err(ObjectErr::InvalidObject {
                        kind: "tag",
                        message: format!("unknown header `{key}`"),
                    });
                }
            }
        }

        Ok(Self {
            object: object.ok_or_else(|| missing("tag", "object"))?,
            object_type: object_type.ok_or_else(|| missing("tag", "type"))?,
            name: name.ok_or_else(|| missing("tag", "name"))?,
            tagger: tagger.build("tagger")?,
            message: message.to_string(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct LooseObjectStore {
    root: PathBuf,
}

impl LooseObjectStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn write(&self, object: &Object) -> Result<ObjectId> {
        self.write_canonical_bytes(&object.canonical_bytes())
    }

    pub fn write_canonical_bytes(&self, bytes: &[u8]) -> Result<ObjectId> {
        let object = Object::decode(bytes)?;
        let expected = object.id();
        let actual = ObjectId::for_bytes(bytes);
        if expected != actual {
            return Err(ObjectErr::ObjectIdMismatch { expected, actual });
        }
        self.write_raw_unchecked(&actual, bytes)?;
        Ok(actual)
    }

    pub fn write_raw_validated(&self, id: &ObjectId, bytes: &[u8]) -> Result<Object> {
        let object = Object::decode(bytes)?;
        let actual = ObjectId::for_bytes(bytes);
        if &actual != id {
            return Err(ObjectErr::ObjectIdMismatch { expected: id.clone(), actual });
        }
        self.write_raw_unchecked(id, bytes)?;
        Ok(object)
    }

    pub fn read(&self, id: &ObjectId) -> Result<Object> {
        let bytes = fs::read(self.path_for(id))?;
        let object = Object::decode(&bytes)?;
        let actual = ObjectId::for_bytes(&bytes);
        if &actual != id {
            return Err(ObjectErr::ObjectIdMismatch { expected: id.clone(), actual });
        }
        Ok(object)
    }

    pub fn read_raw(&self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        let path = self.path_for(id);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(fs::read(path)?))
    }

    pub fn path_for(&self, id: &ObjectId) -> PathBuf {
        let (dir, file) = id.as_str().split_at(2);
        self.root.join(dir).join(file)
    }

    pub fn relative_path(id: &ObjectId) -> String {
        let (dir, file) = id.as_str().split_at(2);
        format!("objects/{dir}/{file}")
    }

    fn write_raw_unchecked(&self, id: &ObjectId, bytes: &[u8]) -> Result<()> {
        let path = self.path_for(id);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)?;
        Ok(())
    }
}

fn validate_object_id(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ObjectErr::InvalidObjectId(value.to_string()));
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\n')
        || path.contains("//")
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(ObjectErr::InvalidObject {
            kind: "tree",
            message: format!("invalid path `{path}`"),
        });
    }
    Ok(())
}

fn parse_field<T: FromStr>(
    value: Option<&str>,
    kind: &'static str,
    field: &'static str,
) -> Result<T>
where
    T::Err: Display,
{
    let value = value.ok_or_else(|| missing(kind, field))?;
    parse_value(value, kind)
}

fn parse_value<T: FromStr>(value: &str, kind: &'static str) -> Result<T>
where
    T::Err: Display,
{
    value
        .parse::<T>()
        .map_err(|err| ObjectErr::InvalidObject { kind, message: err.to_string() })
}

fn ensure_no_extra<'a>(mut parts: impl Iterator<Item = &'a str>, kind: &'static str) -> Result<()> {
    if let Some(extra) = parts.next() {
        return Err(ObjectErr::InvalidObject {
            kind,
            message: format!("unexpected field `{extra}`"),
        });
    }
    Ok(())
}

fn missing(kind: &'static str, field: &str) -> ObjectErr {
    ObjectErr::InvalidObject {
        kind,
        message: format!("missing {field}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_blob_v2_roundtrips_large_text_in_linear_encoding() {
        let bytes = "Markdown 中文内容\n".repeat(16_384).into_bytes();
        let object = Object::Blob(BlobObject::File(FileBlob {
            kind: FileContentKind::TextFile,
            bytes: bytes.clone(),
        }));

        let canonical = object.canonical_bytes();
        assert!(
            canonical
                .windows("file-blob-v2".len())
                .any(|window| window == b"file-blob-v2")
        );
        assert!(canonical.len() < bytes.len() * 2);
        assert_eq!(Object::decode(&canonical).unwrap(), object);
    }

    #[test]
    fn file_blob_v1_remains_readable_with_its_original_object_id() {
        let bytes = b"legacy text";
        let payload = format!(
            "file-blob-v1\nkind text_file\nsize {}\ndata {}\n",
            bytes.len(),
            bs58::encode(bytes).into_string()
        );
        let canonical = format!("graft-object 1 blob {}\0{}", payload.len(), payload);
        let id = ObjectId::for_bytes(canonical.as_bytes());
        let tmp = tempfile::tempdir().unwrap();
        let store = LooseObjectStore::new(tmp.path());

        store
            .write_raw_validated(&id, canonical.as_bytes())
            .unwrap();
        assert_eq!(
            store.read(&id).unwrap(),
            Object::Blob(BlobObject::File(FileBlob {
                kind: FileContentKind::TextFile,
                bytes: bytes.to_vec(),
            }))
        );
    }

    #[test]
    fn object_id_changes_when_tree_changes() {
        let first = Object::Tree(
            TreeObject::new(vec![TreeEntry {
                mode: TreeEntryMode::SqliteDatabase,
                oid: ObjectId::new(
                    "1111111111111111111111111111111111111111111111111111111111111111",
                )
                .unwrap(),
                path: "app.db".to_string(),
            }])
            .unwrap(),
        );
        let second = Object::Tree(
            TreeObject::new(vec![TreeEntry {
                mode: TreeEntryMode::SqliteDatabase,
                oid: ObjectId::new(
                    "2222222222222222222222222222222222222222222222222222222222222222",
                )
                .unwrap(),
                path: "app.db".to_string(),
            }])
            .unwrap(),
        );

        assert_ne!(first.id(), second.id());
    }

    #[test]
    fn loose_store_roundtrips_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LooseObjectStore::new(tmp.path());
        let object = Object::Blob(BlobObject::SqliteSnapshot(SqliteSnapshotBlob {
            volume: VolumeId::random(),
            page_count: PageCount::new(3),
            ranges: vec![SqliteSnapshotRange {
                log: LogId::random(),
                start: LSN::FIRST,
                end: LSN::new(2),
                commits: vec![
                    SqliteSnapshotCommit {
                        lsn: LSN::FIRST,
                        commit_hash: CommitHash::testonly_random(),
                    },
                    SqliteSnapshotCommit {
                        lsn: LSN::new(2),
                        commit_hash: CommitHash::testonly_random(),
                    },
                ],
            }],
        }));

        let id = store.write(&object).unwrap();
        assert_eq!(store.read(&id).unwrap(), object);
        assert!(store.path_for(&id).exists());
    }

    #[test]
    fn sqlite_snapshot_blob_roundtrips_storage_commit_hashes() {
        let volume = VolumeId::random();
        let log = LogId::random();
        let commit_hash = CommitHash::testonly_random();
        let second_commit_hash = CommitHash::testonly_random();
        let object = Object::Blob(BlobObject::SqliteSnapshot(SqliteSnapshotBlob {
            volume: volume.clone(),
            page_count: PageCount::new(3),
            ranges: vec![SqliteSnapshotRange {
                log: log.clone(),
                start: LSN::FIRST,
                end: LSN::new(2),
                commits: vec![
                    SqliteSnapshotCommit { lsn: LSN::FIRST, commit_hash },
                    SqliteSnapshotCommit {
                        lsn: LSN::new(2),
                        commit_hash: second_commit_hash,
                    },
                ],
            }],
        }));

        assert_eq!(Object::decode(&object.canonical_bytes()).unwrap(), object);

        let without_hash = Object::Blob(BlobObject::SqliteSnapshot(SqliteSnapshotBlob {
            volume,
            page_count: PageCount::new(3),
            ranges: vec![SqliteSnapshotRange {
                log,
                start: LSN::FIRST,
                end: LSN::new(2),
                commits: Vec::new(),
            }],
        }));
        assert_ne!(object.id(), without_hash.id());
        assert!(Object::decode(&without_hash.canonical_bytes()).is_err());

        let out_of_order = Object::Blob(BlobObject::SqliteSnapshot(SqliteSnapshotBlob {
            volume: VolumeId::random(),
            page_count: PageCount::new(3),
            ranges: vec![SqliteSnapshotRange {
                log: LogId::random(),
                start: LSN::FIRST,
                end: LSN::new(2),
                commits: vec![
                    SqliteSnapshotCommit {
                        lsn: LSN::new(2),
                        commit_hash: CommitHash::testonly_random(),
                    },
                    SqliteSnapshotCommit {
                        lsn: LSN::FIRST,
                        commit_hash: CommitHash::testonly_random(),
                    },
                ],
            }],
        }));
        assert!(Object::decode(&out_of_order.canonical_bytes()).is_err());
    }
}
