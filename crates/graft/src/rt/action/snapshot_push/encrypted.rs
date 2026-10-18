use super::*;
use sha2::{Digest, Sha256};
use std::io::{Seek, SeekFrom, Write};

/// Publish immutable snapshot dependencies one at a time. No ref or HEAD changes
/// here; the repository still publishes its retained ref candidate last.
pub(super) async fn prepare(
    storage: Arc<FjallStorage>,
    remote: Arc<Remote>,
    commits: Vec<Commit>,
    mut available: BTreeSet<SegmentId>,
    cancellation: Option<CancellationToken>,
) -> Result<PreparedSnapshotPush> {
    let trace = crate::trace::PushTraceSpan::new("sqlite_snapshot_stream_upload");
    let mut segment_bytes = 0_u64;
    for commit in commits {
        cancellation_checkpoint(&cancellation)?;
        let retain = commit
            .segment_idx
            .as_ref()
            .is_some_and(|idx| available.insert(idx.sid.clone()));
        let storage = storage.clone();
        let build_remote = remote.clone();
        let build_cancellation = cancellation.clone();
        // The spawned native compressor owns admission even if its async caller
        // exits. Lock/drain therefore cannot report completion while it is active.
        let operation = Arc::new(remote.begin_encrypted_operation()?);
        let prepared = spawn_blocking(move || {
            let probe = operation.clone();
            crate::repo::with_secret_cancellation(
                Arc::new(move || probe.check_active().is_err()),
                || {
                    let result =
                        build(storage, &build_remote, commit, retain, &build_cancellation)?;
                    operation
                        .check_active()
                        .map_err(crate::remote::RemoteErr::from)?;
                    Ok::<_, GraftErr>(result)
                },
            )
        })
        .await
        .map_err(|_| LogicalErr::Other("encrypted snapshot compressor failed".into()))??;
        cancellation_checkpoint(&cancellation)?;
        if remote
            .buffered_read_limit()
            .is_some_and(|maximum| prepared.commit.encoded_len() > maximum)
        {
            return Err(
                crate::remote::RemoteErr::from(crate::e2ee::EncryptionError::TooLarge).into(),
            );
        }
        if let Some((sid, file, length, digest)) = prepared.segment {
            remote
                .publish_verified_handle(
                    &format!("segments/{}", sid.serialize()),
                    file,
                    length,
                    digest,
                )
                .await?;
            segment_bytes = segment_bytes
                .checked_add(length)
                .ok_or_else(|| LogicalErr::Other("snapshot byte count exceeds u64".into()))?;
        }
        upload_snapshot_commit(remote.clone(), prepared.commit).await?;
    }
    trace.finish(&[("segment_bytes", segment_bytes)]);
    // Dependencies are already confirmed; no retained plaintext bundle payload.
    Ok(PreparedSnapshotPush::default())
}

struct StreamedUpload {
    commit: Commit,
    segment: Option<(SegmentId, std::fs::File, u64, [u8; 32])>,
}

fn build(
    storage: Arc<FjallStorage>,
    remote: &Remote,
    commit: Commit,
    retain: bool,
    cancellation: &Option<CancellationToken>,
) -> Result<StreamedUpload> {
    let reader = storage.read();
    let Some(idx) = commit.segment_idx.clone() else {
        let hash = CommitHashBuilder::new(
            commit.log.clone(),
            commit.lsn,
            commit.page_count,
            PageCount::ZERO,
        )
        .build();
        return Ok(StreamedUpload {
            commit: commit.with_commit_hash(Some(hash)),
            segment: None,
        });
    };
    if !retain && !idx.frames.is_empty() {
        let hash = match commit.commit_hash.clone() {
            Some(hash) => hash,
            None => commit_hash_for_segment_idx(&reader, &commit, &idx, cancellation)?,
        };
        return Ok(StreamedUpload {
            commit: commit.with_commit_hash(Some(hash)),
            segment: None,
        });
    }
    let mut hash = CommitHashBuilder::new(
        commit.log.clone(),
        commit.lsn,
        commit.page_count,
        idx.page_count(),
    );
    let mut builder = if retain {
        SegmentBuilder::new()
    } else {
        SegmentBuilder::new().discard_chunks()
    };
    let mut spool = if retain {
        Some(remote.encrypted_plaintext_spool()?)
    } else {
        None
    };
    let mut length = 0_u64;
    let mut digest = Sha256::new();
    for pageidx in idx.pageset.iter() {
        cancellation_checkpoint(cancellation)?;
        let page = reader.read_page(idx.sid.clone(), pageidx)?.ok_or_else(|| {
            LogicalErr::Other("snapshot references an unavailable local page".into())
        })?;
        hash.write_page(pageidx, &page);
        builder.write(pageidx, &page);
        if let Some((file, maximum)) = spool.as_mut() {
            write_chunks(
                file,
                *maximum,
                builder.drain_chunks(),
                &mut length,
                &mut digest,
            )?;
        }
    }
    let (frames, chunks) = builder.finish();
    if let Some((file, maximum)) = spool.as_mut() {
        write_chunks(file, *maximum, chunks, &mut length, &mut digest)?;
        file.seek(SeekFrom::Start(0))
            .map_err(crate::remote::RemoteErr::from)?;
    }
    let sid = idx.sid.clone();
    Ok(StreamedUpload {
        commit: commit
            .with_commit_hash(Some(hash.build()))
            .with_segment_idx(Some(idx.with_frames(frames))),
        segment: spool.map(|(file, _)| (sid, file, length, digest.finalize().into())),
    })
}

fn write_chunks(
    file: &mut std::fs::File,
    maximum: u64,
    chunks: impl IntoIterator<Item = Bytes>,
    length: &mut u64,
    digest: &mut Sha256,
) -> Result<()> {
    for chunk in chunks {
        *length = length
            .checked_add(chunk.len() as u64)
            .filter(|length| *length <= maximum)
            .ok_or_else(|| {
                crate::remote::RemoteErr::from(crate::e2ee::EncryptionError::TooLarge)
            })?;
        file.write_all(&chunk)
            .map_err(crate::remote::RemoteErr::from)?;
        digest.update(&chunk);
    }
    Ok(())
}
