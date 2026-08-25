//! Repository-independent fixed-page deltas for consistent `SQLite` images.

use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
};

use graft::{
    core::{PageIdx, page::PAGESIZE},
    volume_reader::VolumeRead,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{error::ErrCtx, pragma::sqlite_worktree::PhysicalSqliteReader};

pub const SQLITE_PAGE_DELTA_FORMAT: &str = "graft-sqlite-page-delta-v1";
pub(crate) const SQLITE_PAGE_DELTA_MAGIC: &[u8; 8] = b"GRAFTD01";
pub const SQLITE_PAGE_DELTA_HEADER_BYTES: u32 = 104;
const SQLITE_PAGE_DELTA_FLAGS: u32 = 0;
const SHA256_BYTES: usize = 32;

/// Self-contained metadata embedded in one `GRAFTD01` delta.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SqlitePageDeltaMetadata {
    pub format: String,
    pub header_bytes: u32,
    pub flags: u32,
    pub page_bytes: u32,
    pub base_bytes: u64,
    pub target_bytes: u64,
    pub changed_pages: u32,
    pub base_sha256: String,
    pub target_sha256: String,
    pub delta_bytes: u64,
    pub beneficial: bool,
}

#[derive(Debug, Clone)]
struct DeltaHeader {
    page_bytes: u32,
    base_bytes: u64,
    target_bytes: u64,
    changed_pages: u32,
    base_sha256: [u8; SHA256_BYTES],
    target_sha256: [u8; SHA256_BYTES],
}

impl DeltaHeader {
    fn metadata(&self, delta_bytes: u64) -> SqlitePageDeltaMetadata {
        SqlitePageDeltaMetadata {
            format: SQLITE_PAGE_DELTA_FORMAT.to_string(),
            header_bytes: SQLITE_PAGE_DELTA_HEADER_BYTES,
            flags: SQLITE_PAGE_DELTA_FLAGS,
            page_bytes: self.page_bytes,
            base_bytes: self.base_bytes,
            target_bytes: self.target_bytes,
            changed_pages: self.changed_pages,
            base_sha256: sha256_hex(&self.base_sha256),
            target_sha256: sha256_hex(&self.target_sha256),
            delta_bytes,
            beneficial: delta_bytes < self.target_bytes,
        }
    }
}

/// Creates a portable delta between two consistent physical `SQLite` snapshots.
pub fn create_sqlite_page_delta(
    base: &Path,
    target: &Path,
    output: &Path,
) -> Result<SqlitePageDeltaMetadata, ErrCtx> {
    let base_reader = PhysicalSqliteReader::open_stable(base)?;
    let target_reader = PhysicalSqliteReader::open_stable(target)?;
    let base_sha256 = hash_reader(&base_reader)?;
    let target_sha256 = hash_reader(&target_reader)?;
    write_delta_from_readers(
        &base_reader,
        &target_reader,
        output,
        base_sha256,
        target_sha256,
        None,
    )
}

/// Applies a delta to the exact base image named by its embedded digest.
pub fn apply_sqlite_page_delta(
    base: &Path,
    delta: &Path,
    output: &Path,
) -> Result<SqlitePageDeltaMetadata, ErrCtx> {
    let base_reader = PhysicalSqliteReader::open_stable(base)?;
    let mut delta_reader = BufReader::new(File::open(delta)?);
    let delta_bytes = std::fs::metadata(delta)?.len();
    let header = read_and_validate_header(&mut delta_reader, delta_bytes)?;
    validate_entries(&mut delta_reader, &header)?;

    if reader_bytes(&base_reader)? != header.base_bytes
        || hash_reader(&base_reader)? != header.base_sha256
    {
        return Err(invalid_delta(
            "SQLite delta does not match the supplied base image",
        ));
    }

    delta_reader.seek(SeekFrom::Start(u64::from(SQLITE_PAGE_DELTA_HEADER_BYTES)))?;
    let mut next_patch = read_patch(&mut delta_reader, &header, 0)?;
    let mut output_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let write_result = (|| -> Result<[u8; SHA256_BYTES], ErrCtx> {
        let mut writer = BufWriter::new(&mut output_file);
        let mut hasher = Sha256::new();
        let target_pages = page_count(header.target_bytes, header.page_bytes)?;
        let base_pages = base_reader.page_count().to_u32();
        for page_number in 1..=target_pages {
            graft::repo::cancellation_checkpoint()?;
            let bytes = if next_patch
                .as_ref()
                .is_some_and(|(patch_page, _)| *patch_page == page_number)
            {
                let (_, bytes) = next_patch.take().expect("matching patch is present");
                next_patch = read_patch(&mut delta_reader, &header, page_number)?;
                bytes
            } else if page_number <= base_pages {
                let page_idx = PageIdx::try_new(page_number).expect("page number is non-zero");
                base_reader.read_page(page_idx)?.as_ref().to_vec()
            } else {
                return Err(invalid_delta(format!(
                    "SQLite delta is missing new target page {page_number}"
                )));
            };
            hasher.update(&bytes);
            writer.write_all(&bytes)?;
        }
        if next_patch.is_some() {
            return Err(invalid_delta("SQLite delta contains unused page entries"));
        }
        writer.flush()?;
        drop(writer);
        output_file.sync_all()?;
        Ok(hasher.finalize().into())
    })();
    drop(output_file);
    match write_result {
        Ok(target_sha256) if target_sha256 == header.target_sha256 => {
            Ok(header.metadata(delta_bytes))
        }
        Ok(_) => {
            let _ = std::fs::remove_file(output);
            Err(invalid_delta(
                "SQLite delta materialized a target with the wrong SHA-256",
            ))
        }
        Err(error) => {
            let _ = std::fs::remove_file(output);
            Err(error)
        }
    }
}

/// Validates a delta and returns its embedded metadata without applying it.
pub fn inspect_sqlite_page_delta(delta: &Path) -> Result<SqlitePageDeltaMetadata, ErrCtx> {
    let delta_bytes = std::fs::metadata(delta)?.len();
    let mut reader = BufReader::new(File::open(delta)?);
    let header = read_and_validate_header(&mut reader, delta_bytes)?;
    validate_entries(&mut reader, &header)?;
    Ok(header.metadata(delta_bytes))
}

pub(crate) fn write_delta_from_readers(
    base: &dyn VolumeRead,
    target: &dyn VolumeRead,
    output: &Path,
    base_sha256: [u8; SHA256_BYTES],
    target_sha256: [u8; SHA256_BYTES],
    candidates: Option<Vec<u32>>,
) -> Result<SqlitePageDeltaMetadata, ErrCtx> {
    let base_pages = base.page_count().to_u32();
    let target_pages = target.page_count().to_u32();
    let mut pages = candidates.map_or_else(
        || (1..=target_pages).collect(),
        |pages| pages.into_iter().collect::<BTreeSet<_>>(),
    );
    if target_pages > base_pages {
        pages.extend((base_pages + 1)..=target_pages);
    }

    let mut changed_pages = Vec::new();
    for page_number in pages {
        graft::repo::cancellation_checkpoint()?;
        if page_number == 0 || page_number > target_pages {
            continue;
        }
        let page_idx = PageIdx::try_new(page_number).expect("page number is non-zero");
        let target_page = target.read_page(page_idx)?;
        if page_number > base_pages || base.read_page(page_idx)? != target_page {
            changed_pages.push(page_number);
        }
    }
    let changed_page_count = u32::try_from(changed_pages.len())
        .map_err(|_| invalid_delta("SQLite delta has too many changed pages"))?;
    let header = DeltaHeader {
        page_bytes: PAGESIZE.as_u32(),
        base_bytes: u64::from(base_pages) * PAGESIZE.as_u64(),
        target_bytes: u64::from(target_pages) * PAGESIZE.as_u64(),
        changed_pages: changed_page_count,
        base_sha256,
        target_sha256,
    };

    let mut output_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let write_result = (|| -> Result<(), ErrCtx> {
        write_header(&mut output_file, &header)?;
        for page_number in changed_pages {
            graft::repo::cancellation_checkpoint()?;
            let page_idx = PageIdx::try_new(page_number).expect("page number is non-zero");
            output_file.write_all(&page_number.to_le_bytes())?;
            output_file.write_all(target.read_page(page_idx)?.as_ref())?;
        }
        output_file.flush()?;
        output_file.sync_all()?;
        Ok(())
    })();
    drop(output_file);
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(output);
        return Err(error);
    }
    let delta_bytes = std::fs::metadata(output)?.len();
    Ok(header.metadata(delta_bytes))
}

pub(crate) fn sha256_hex_bytes(value: &[u8; SHA256_BYTES]) -> String {
    sha256_hex(value)
}

pub(crate) fn parse_sha256_hex(value: &str) -> Result<[u8; SHA256_BYTES], ErrCtx> {
    if value.len() != SHA256_BYTES * 2 {
        return Err(invalid_delta(
            "SHA-256 must contain 64 lowercase hexadecimal characters",
        ));
    }
    let mut bytes = [0_u8; SHA256_BYTES];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Ok(bytes)
}

fn hex_nibble(value: u8) -> Result<u8, ErrCtx> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(invalid_delta(
            "SHA-256 must contain 64 lowercase hexadecimal characters",
        )),
    }
}

fn hash_reader(reader: &dyn VolumeRead) -> Result<[u8; SHA256_BYTES], ErrCtx> {
    let mut hasher = Sha256::new();
    for page_idx in reader.page_count().iter() {
        graft::repo::cancellation_checkpoint()?;
        hasher.update(reader.read_page(page_idx)?.as_ref());
    }
    Ok(hasher.finalize().into())
}

fn reader_bytes(reader: &dyn VolumeRead) -> Result<u64, ErrCtx> {
    u64::from(reader.page_count().to_u32())
        .checked_mul(PAGESIZE.as_u64())
        .ok_or_else(|| invalid_delta("SQLite image size overflows u64"))
}

fn write_header(writer: &mut File, header: &DeltaHeader) -> Result<(), ErrCtx> {
    writer.write_all(SQLITE_PAGE_DELTA_MAGIC)?;
    writer.write_all(&SQLITE_PAGE_DELTA_HEADER_BYTES.to_le_bytes())?;
    writer.write_all(&SQLITE_PAGE_DELTA_FLAGS.to_le_bytes())?;
    writer.write_all(&header.page_bytes.to_le_bytes())?;
    writer.write_all(&header.changed_pages.to_le_bytes())?;
    writer.write_all(&header.base_bytes.to_le_bytes())?;
    writer.write_all(&header.target_bytes.to_le_bytes())?;
    writer.write_all(&header.base_sha256)?;
    writer.write_all(&header.target_sha256)?;
    Ok(())
}

fn read_and_validate_header(
    reader: &mut BufReader<File>,
    delta_bytes: u64,
) -> Result<DeltaHeader, ErrCtx> {
    let mut raw = [0_u8; SQLITE_PAGE_DELTA_HEADER_BYTES as usize];
    reader
        .read_exact(&mut raw)
        .map_err(|_| invalid_delta("SQLite delta header is incomplete"))?;
    if &raw[..8] != SQLITE_PAGE_DELTA_MAGIC {
        return Err(invalid_delta("SQLite delta magic is invalid"));
    }
    let header_bytes = u32::from_le_bytes(raw[8..12].try_into().expect("four bytes"));
    let flags = u32::from_le_bytes(raw[12..16].try_into().expect("four bytes"));
    let page_bytes = u32::from_le_bytes(raw[16..20].try_into().expect("four bytes"));
    let changed_pages = u32::from_le_bytes(raw[20..24].try_into().expect("four bytes"));
    let base_bytes = u64::from_le_bytes(raw[24..32].try_into().expect("eight bytes"));
    let target_bytes = u64::from_le_bytes(raw[32..40].try_into().expect("eight bytes"));
    let base_sha256 = raw[40..72].try_into().expect("32 bytes");
    let target_sha256 = raw[72..104].try_into().expect("32 bytes");
    if header_bytes != SQLITE_PAGE_DELTA_HEADER_BYTES
        || flags != SQLITE_PAGE_DELTA_FLAGS
        || page_bytes != PAGESIZE.as_u32()
        || base_bytes == 0
        || target_bytes == 0
        || base_bytes % u64::from(page_bytes) != 0
        || target_bytes % u64::from(page_bytes) != 0
        || changed_pages > page_count(target_bytes, page_bytes)?
    {
        return Err(invalid_delta("SQLite delta header is invalid"));
    }
    let expected_bytes = u64::from(header_bytes)
        .checked_add(
            u64::from(changed_pages)
                .checked_mul(u64::from(page_bytes) + 4)
                .ok_or_else(|| invalid_delta("SQLite delta size overflows u64"))?,
        )
        .ok_or_else(|| invalid_delta("SQLite delta size overflows u64"))?;
    if expected_bytes != delta_bytes {
        return Err(invalid_delta(
            "SQLite delta length does not match its header",
        ));
    }
    Ok(DeltaHeader {
        page_bytes,
        base_bytes,
        target_bytes,
        changed_pages,
        base_sha256,
        target_sha256,
    })
}

fn validate_entries(reader: &mut BufReader<File>, header: &DeltaHeader) -> Result<(), ErrCtx> {
    let mut previous_page = 0;
    let target_pages = page_count(header.target_bytes, header.page_bytes)?;
    for _ in 0..header.changed_pages {
        let mut page_number = [0_u8; 4];
        reader.read_exact(&mut page_number)?;
        let page_number = u32::from_le_bytes(page_number);
        if page_number <= previous_page || page_number > target_pages {
            return Err(invalid_delta(
                "SQLite delta entries must be sorted, unique, and inside the target",
            ));
        }
        reader.seek(SeekFrom::Current(i64::from(header.page_bytes)))?;
        previous_page = page_number;
    }
    Ok(())
}

fn read_patch(
    reader: &mut BufReader<File>,
    header: &DeltaHeader,
    previous_page: u32,
) -> Result<Option<(u32, Vec<u8>)>, ErrCtx> {
    if reader.stream_position()?
        >= u64::from(SQLITE_PAGE_DELTA_HEADER_BYTES)
            + u64::from(header.changed_pages) * (u64::from(header.page_bytes) + 4)
    {
        return Ok(None);
    }
    let mut raw_page = [0_u8; 4];
    reader.read_exact(&mut raw_page)?;
    let page_number = u32::from_le_bytes(raw_page);
    let target_pages = page_count(header.target_bytes, header.page_bytes)?;
    if page_number <= previous_page || page_number > target_pages {
        return Err(invalid_delta(
            "SQLite delta entries must be sorted, unique, and inside the target",
        ));
    }
    let mut bytes = vec![0_u8; header.page_bytes as usize];
    reader.read_exact(&mut bytes)?;
    Ok(Some((page_number, bytes)))
}

fn page_count(bytes: u64, page_bytes: u32) -> Result<u32, ErrCtx> {
    let pages = bytes / u64::from(page_bytes);
    u32::try_from(pages).map_err(|_| invalid_delta("SQLite delta has too many pages"))
}

fn sha256_hex(value: &[u8; SHA256_BYTES]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn invalid_delta(message: impl Into<String>) -> ErrCtx {
    ErrCtx::InvalidCommand(message.into().into())
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};

    use super::*;

    fn create_database(path: &Path, rows: u32) {
        let mut connection = Connection::open(path).unwrap();
        connection
            .pragma_update(None, "page_size", PAGESIZE.as_u32())
            .unwrap();
        connection
            .execute_batch("CREATE TABLE records(id INTEGER PRIMARY KEY, value BLOB NOT NULL);")
            .unwrap();
        let transaction = connection.transaction().unwrap();
        for id in 1..=rows {
            transaction
                .execute(
                    "INSERT INTO records(id, value) VALUES (?1, ?2)",
                    params![id, vec![id as u8; 2048]],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
    }

    #[test]
    fn creates_inspects_and_applies_a_delta() {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().join("base.sqlite");
        let target = directory.path().join("target.sqlite");
        let delta = directory.path().join("update.graft-delta");
        let restored = directory.path().join("restored.sqlite");
        create_database(&base, 8);
        std::fs::copy(&base, &target).unwrap();
        let target_connection = Connection::open(&target).unwrap();
        target_connection
            .execute(
                "UPDATE records SET value = ?1 WHERE id = 4",
                params![vec![91_u8; 2048]],
            )
            .unwrap();
        drop(target_connection);

        let created = create_sqlite_page_delta(&base, &target, &delta).unwrap();
        assert_eq!(created.format, SQLITE_PAGE_DELTA_FORMAT);
        assert!(created.changed_pages > 0);
        assert_eq!(inspect_sqlite_page_delta(&delta).unwrap(), created);

        let applied = apply_sqlite_page_delta(&base, &delta, &restored).unwrap();
        assert_eq!(applied.target_sha256, created.target_sha256);
        let restored_connection = Connection::open(&restored).unwrap();
        let value: Vec<u8> = restored_connection
            .query_row("SELECT value FROM records WHERE id = 4", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(value, vec![91_u8; 2048]);
    }

    #[test]
    fn rejects_the_wrong_base_and_preserves_existing_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().join("base.sqlite");
        let wrong_base = directory.path().join("wrong.sqlite");
        let target = directory.path().join("target.sqlite");
        let delta = directory.path().join("update.graft-delta");
        let output = directory.path().join("output.sqlite");
        create_database(&base, 3);
        create_database(&wrong_base, 4);
        create_database(&target, 5);
        create_sqlite_page_delta(&base, &target, &delta).unwrap();
        std::fs::write(&output, b"keep me").unwrap();

        assert!(apply_sqlite_page_delta(&wrong_base, &delta, &output).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), b"keep me");
    }

    #[test]
    fn rejects_corrupt_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().join("base.sqlite");
        let target = directory.path().join("target.sqlite");
        let delta = directory.path().join("update.graft-delta");
        create_database(&base, 2);
        create_database(&target, 3);
        create_sqlite_page_delta(&base, &target, &delta).unwrap();
        let mut bytes = std::fs::read(&delta).unwrap();
        bytes[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&delta, bytes).unwrap();

        assert!(inspect_sqlite_page_delta(&delta).is_err());
    }

    #[test]
    fn delta_identity_uses_exact_snapshot_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().join("base.sqlite");
        let target = directory.path().join("target.sqlite");
        let delta = directory.path().join("update.graft-delta");
        let restored = directory.path().join("restored.sqlite");
        create_database(&base, 2);
        let base_connection = Connection::open(&base).unwrap();
        base_connection
            .pragma_update(None, "journal_mode", "WAL")
            .unwrap();
        base_connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        drop(base_connection);
        let base_bytes = std::fs::read(&base).unwrap();
        std::fs::copy(&base, &target).unwrap();
        let target_connection = Connection::open(&target).unwrap();
        target_connection
            .execute(
                "UPDATE records SET value = ?1 WHERE id = 1",
                params![vec![55_u8; 2048]],
            )
            .unwrap();
        target_connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        drop(target_connection);

        let expected_base_sha256: [u8; SHA256_BYTES] = Sha256::digest(&base_bytes).into();
        let created = create_sqlite_page_delta(&base, &target, &delta).unwrap();

        assert_eq!(created.base_sha256, sha256_hex(&expected_base_sha256));
        apply_sqlite_page_delta(&base, &delta, &restored).unwrap();
        assert_eq!(
            std::fs::read(restored).unwrap(),
            std::fs::read(target).unwrap()
        );
    }
}
