use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use bytes::Bytes;
use bilrost::Message;
use futures::{StreamExt, TryStreamExt, stream};
use tokio::task::spawn_blocking;

use crate::core::{
    CommitHashBuilder, LogId, PageCount, SegmentId, commit_hash::CommitHash,
    commit::{Commit, SegmentIdx},
    lsn::LSN,
};
use crate::{
    GraftErr, LogicalErr,
    local::fjall_storage::FjallStorage,
    remote::{Remote, segment::SegmentBuilder},
    rt::action::{Action, Result},
    snapshot::Snapshot,
};

const SEGMENT_EXISTS_CONCURRENCY: usize = 5;

/// Publishes the commits and segments referenced by a snapshot to a remote,
/// preserving the snapshot's original log IDs and LSNs.
#[derive(Debug)]
pub struct SnapshotPush {
    pub snapshot: Snapshot,
}

#[derive(Debug)]
pub struct SnapshotsPush {
    pub snapshots: Vec<Snapshot>,
}

#[derive(Debug)]
struct SnapshotUpload {
    commit: Commit,
    segment: Option<(SegmentId, Vec<Bytes>)>,
}

#[derive(Debug, Default)]
pub struct PreparedSnapshotPush {
    uploads: Vec<SnapshotUpload>,
    segment_bytes: u64,
}

impl PreparedSnapshotPush {
    pub(crate) fn into_bundle_objects(
        self,
    ) -> crate::remote::Result<Vec<crate::remote::RemoteBundleObject>> {
        let mut objects = Vec::new();
        for upload in self.uploads {
            if let Some((sid, chunks)) = upload.segment {
                objects.push(crate::remote::RemoteBundleObject::new(
                    format!("segments/{}", sid.serialize()),
                    chunks,
                    true,
                )?);
            }
            let commit_path = format!(
                "logs/{}/commits/{}",
                upload.commit.log().serialize(),
                crate::core::cbe::CBE64::from(upload.commit.lsn()),
            );
            objects.push(crate::remote::RemoteBundleObject::new(
                commit_path,
                [upload.commit.encode_to_bytes()],
                false,
            )?);
        }
        Ok(objects)
    }

    async fn publish(self, remote: Arc<Remote>) -> Result<()> {
        let upload_trace = crate::trace::PushTraceSpan::new("sqlite_snapshot_http_upload");
        let upload_concurrency = remote.snapshot_upload_concurrency();
        let result = stream::iter(self.uploads)
            .map(|upload| {
                let remote = remote.clone();
                async move { upload_snapshot_upload(remote, upload).await }
            })
            .buffer_unordered(upload_concurrency)
            .try_collect()
            .await;
        if result.is_ok() {
            upload_trace.finish(&[("segment_bytes", self.segment_bytes)]);
        }
        result
    }
}

impl Action for SnapshotPush {
    async fn run(self, storage: Arc<FjallStorage>, remote: Arc<Remote>) -> Result<()> {
        prepare_snapshots(storage, remote.clone(), vec![self.snapshot])
            .await?
            .publish(remote)
            .await
    }
}

impl Action for SnapshotsPush {
    async fn run(self, storage: Arc<FjallStorage>, remote: Arc<Remote>) -> Result<()> {
        prepare_snapshots(storage, remote.clone(), self.snapshots)
            .await?
            .publish(remote)
            .await
    }
}

pub(crate) async fn prepare_snapshots(
    storage: Arc<FjallStorage>,
    remote: Arc<Remote>,
    snapshots: Vec<Snapshot>,
) -> Result<PreparedSnapshotPush> {
    if snapshots.is_empty() {
        return Ok(PreparedSnapshotPush::default());
    }

    let collect_trace = crate::trace::PushTraceSpan::new("sqlite_snapshot_collect");
    let commits = spawn_blocking({
        let storage = storage.clone();
        move || collect_snapshots_commits(storage, snapshots)
    })
    .await
    .expect("snapshot upload commit collection task failed")?;
    collect_trace.finish(&[("commits", commits.len() as u64)]);
    let negotiation_trace = crate::trace::PushTraceSpan::new("sqlite_segment_negotiation");
    let existing_segments = if commits.len() > 1 {
        existing_remote_segments(remote.clone(), &commits).await?
    } else {
        BTreeSet::new()
    };
    negotiation_trace.finish(&[("existing_segments", existing_segments.len() as u64)]);
    let build_trace = crate::trace::PushTraceSpan::new("sqlite_snapshot_build");
    let uploads = spawn_blocking(move || build_uploads(storage, commits, existing_segments))
        .await
        .expect("snapshot upload build task failed")?;
    let segment_bytes = uploads
        .iter()
        .filter_map(|upload| upload.segment.as_ref())
        .flat_map(|(_, chunks)| chunks)
        .map(|chunk| chunk.len() as u64)
        .sum();
    build_trace.finish(&[
        ("uploads", uploads.len() as u64),
        ("segment_bytes", segment_bytes),
    ]);

    Ok(PreparedSnapshotPush { uploads, segment_bytes })
}

async fn upload_snapshot_upload(remote: Arc<Remote>, upload: SnapshotUpload) -> Result<()> {
    if let Some((sid, chunks)) = upload.segment {
        remote.put_segment(&sid, chunks).await?;
    }

    upload_snapshot_commit(remote, upload.commit).await
}

async fn upload_snapshot_commit(remote: Arc<Remote>, commit: Commit) -> Result<()> {
    match remote.put_commit(&commit).await {
        Ok(()) => Ok(()),
        Err(err) if err.precondition_failed() => {
            let existing = remote.get_commit(commit.log(), commit.lsn()).await?;
            if existing.as_ref() != Some(&commit) {
                return Err(LogicalErr::Other(format!(
                    "remote already has a different commit for {:?}/{}",
                    commit.log(),
                    commit.lsn()
                ))
                .into());
            }
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

fn collect_snapshots_commits(
    storage: Arc<FjallStorage>,
    snapshots: Vec<Snapshot>,
) -> std::result::Result<Vec<Commit>, GraftErr> {
    let reader = storage.read();
    let mut commits = BTreeMap::<(LogId, LSN), Commit>::new();
    for snapshot in snapshots {
        let mut snapshot_commits = reader
            .commits(&snapshot)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        snapshot_commits.reverse();
        for commit in snapshot_commits {
            let key = (commit.log.clone(), commit.lsn);
            if let Some(existing) = commits.get(&key) {
                if existing != &commit {
                    return Err(LogicalErr::Other(format!(
                        "snapshot commit {:?}/{} was read with conflicting metadata",
                        commit.log, commit.lsn
                    ))
                    .into());
                }
                continue;
            }
            commits.insert(key, commit);
        }
    }
    Ok(commits.into_values().collect())
}

async fn existing_remote_segments(
    remote: Arc<Remote>,
    commits: &[Commit],
) -> Result<BTreeSet<SegmentId>> {
    let mut sids = BTreeSet::new();
    for commit in commits {
        if let Some(segment_idx) = commit.segment_idx() {
            sids.insert(segment_idx.sid.clone());
        }
    }

    stream::iter(sids)
        .map(|sid| {
            let remote = remote.clone();
            async move {
                let exists = remote.has_segment(&sid).await?;
                Ok((sid, exists))
            }
        })
        .buffer_unordered(SEGMENT_EXISTS_CONCURRENCY)
        .try_filter_map(|(sid, exists)| async move { Ok(exists.then_some(sid)) })
        .try_collect()
        .await
}

fn build_uploads(
    storage: Arc<FjallStorage>,
    commits: Vec<Commit>,
    existing_segments: BTreeSet<SegmentId>,
) -> std::result::Result<Vec<SnapshotUpload>, GraftErr> {
    let reader = storage.read();
    let mut available_or_planned_segments = existing_segments;
    let mut uploads = Vec::with_capacity(commits.len());
    for commit in commits {
        let Some(segment_idx) = commit.segment_idx.clone() else {
            let commit_hash = CommitHashBuilder::new(
                commit.log.clone(),
                commit.lsn,
                commit.page_count,
                PageCount::ZERO,
            )
            .build();
            uploads.push(SnapshotUpload {
                commit: commit.with_commit_hash(Some(commit_hash)),
                segment: None,
            });
            continue;
        };

        let sid = segment_idx.sid.clone();
        let retain_chunks = available_or_planned_segments.insert(sid.clone());
        let segment_available_or_planned = !retain_chunks;
        if segment_available_or_planned && !segment_idx.frames.is_empty() {
            let commit_hash = match commit.commit_hash.clone() {
                Some(commit_hash) => commit_hash,
                None => commit_hash_for_segment_idx(&reader, &commit, &segment_idx)?,
            };
            uploads.push(SnapshotUpload {
                commit: commit
                    .with_commit_hash(Some(commit_hash))
                    .with_segment_idx(Some(segment_idx)),
                segment: None,
            });
            continue;
        }

        let (segment_idx, chunks, commit_hash) =
            build_segment_for_commit(&reader, &commit, segment_idx, retain_chunks)?;
        let sid = segment_idx.sid.clone();
        uploads.push(SnapshotUpload {
            commit: commit
                .with_commit_hash(Some(commit_hash))
                .with_segment_idx(Some(segment_idx)),
            segment: retain_chunks.then_some((sid, chunks)),
        });
    }

    Ok(uploads)
}

fn commit_hash_for_segment_idx(
    reader: &crate::local::fjall_storage::ReadGuard<'_>,
    commit: &Commit,
    segment_idx: &SegmentIdx,
) -> std::result::Result<CommitHash, GraftErr> {
    let mut hash_builder = CommitHashBuilder::new(
        commit.log.clone(),
        commit.lsn,
        commit.page_count,
        segment_idx.page_count(),
    );

    for pageidx in segment_idx.pageset.iter() {
        let page = reader
            .read_page(segment_idx.sid.clone(), pageidx)?
            .ok_or_else(|| {
                LogicalErr::Other(format!(
                    "snapshot commit {:?}/{} references missing page {:?} in segment {:?}",
                    commit.log, commit.lsn, pageidx, segment_idx.sid
                ))
            })?;
        hash_builder.write_page(pageidx, &page);
    }

    Ok(hash_builder.build())
}

fn build_segment_for_commit(
    reader: &crate::local::fjall_storage::ReadGuard<'_>,
    commit: &Commit,
    segment_idx: SegmentIdx,
    retain_chunks: bool,
) -> std::result::Result<(SegmentIdx, Vec<Bytes>, CommitHash), GraftErr> {
    let mut segment_builder = if retain_chunks {
        SegmentBuilder::new()
    } else {
        SegmentBuilder::new().discard_chunks()
    };
    let mut hash_builder = CommitHashBuilder::new(
        commit.log.clone(),
        commit.lsn,
        commit.page_count,
        segment_idx.page_count(),
    );

    for pageidx in segment_idx.pageset.iter() {
        let page = reader
            .read_page(segment_idx.sid.clone(), pageidx)?
            .ok_or_else(|| {
                LogicalErr::Other(format!(
                    "snapshot commit {:?}/{} references missing page {:?} in segment {:?}",
                    commit.log, commit.lsn, pageidx, segment_idx.sid
                ))
            })?;
        hash_builder.write_page(pageidx, &page);
        segment_builder.write(pageidx, &page);
    }

    let (frames, chunks) = segment_builder.finish();
    let commit_hash = hash_builder.build();
    Ok((segment_idx.with_frames(frames), chunks, commit_hash))
}
