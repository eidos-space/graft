use std::sync::Arc;

use bytes::Bytes;
use tokio::task::spawn_blocking;

use crate::core::{
    CommitHashBuilder, PageCount, SegmentId,
    commit::{Commit, SegmentIdx},
};
use crate::{
    GraftErr, LogicalErr,
    local::fjall_storage::FjallStorage,
    remote::{Remote, segment::SegmentBuilder},
    rt::action::{Action, Result},
    snapshot::Snapshot,
};

/// Publishes the commits and segments referenced by a snapshot to a remote,
/// preserving the snapshot's original log IDs and LSNs.
#[derive(Debug)]
pub struct SnapshotPush {
    pub snapshot: Snapshot,
}

struct SnapshotUpload {
    commit: Commit,
    segment: Option<(SegmentId, Vec<Bytes>)>,
}

impl Action for SnapshotPush {
    async fn run(self, storage: Arc<FjallStorage>, remote: Arc<Remote>) -> Result<()> {
        let uploads = spawn_blocking(move || build_uploads(storage, self.snapshot))
            .await
            .expect("snapshot upload build task failed")?;

        for upload in uploads {
            if let Some((sid, chunks)) = upload.segment {
                remote.put_segment(&sid, chunks).await?;
            }

            match remote.put_commit(&upload.commit).await {
                Ok(()) => {}
                Err(err) if err.precondition_failed() => {
                    let existing = remote
                        .get_commit(upload.commit.log(), upload.commit.lsn())
                        .await?;
                    if existing.as_ref() != Some(&upload.commit) {
                        return Err(LogicalErr::Other(format!(
                            "remote already has a different commit for {:?}/{}",
                            upload.commit.log(),
                            upload.commit.lsn()
                        ))
                        .into());
                    }
                }
                Err(err) => return Err(err.into()),
            }
        }

        Ok(())
    }
}

fn build_uploads(
    storage: Arc<FjallStorage>,
    snapshot: Snapshot,
) -> std::result::Result<Vec<SnapshotUpload>, GraftErr> {
    let reader = storage.read();
    let mut commits = reader
        .commits(&snapshot)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    commits.reverse();

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

        let (segment_idx, chunks, commit_hash) =
            build_segment_for_commit(&reader, &commit, segment_idx)?;
        let sid = segment_idx.sid.clone();
        uploads.push(SnapshotUpload {
            commit: commit
                .with_commit_hash(Some(commit_hash))
                .with_segment_idx(Some(segment_idx)),
            segment: Some((sid, chunks)),
        });
    }

    Ok(uploads)
}

fn build_segment_for_commit(
    reader: &crate::local::fjall_storage::ReadGuard<'_>,
    commit: &Commit,
    segment_idx: SegmentIdx,
) -> std::result::Result<(SegmentIdx, Vec<Bytes>, crate::core::commit_hash::CommitHash), GraftErr>
{
    let mut segment_builder = SegmentBuilder::new();
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
