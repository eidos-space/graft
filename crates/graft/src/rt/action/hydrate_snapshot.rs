use std::sync::Arc;

use futures::{StreamExt, TryStreamExt};
use itertools::Itertools;

use crate::{
    GraftErr,
    local::fjall_storage::FjallStorage,
    remote::Remote,
    rt::action::{Action, fetch_segment::FetchSegment},
    snapshot::Snapshot,
    core::commit::SegmentRangeRef,
};

const HYDRATE_CONCURRENCY: usize = 5;

/// A frozen set of whole compressed frames, shared across all target snapshots.
/// Execution uses this inventory directly, so overlapping snapshots cannot
/// change the declared budget by discovering a different request grouping.
#[derive(Debug)]
pub struct PreparedSnapshotDownload {
    pub(crate) snapshots: Vec<Snapshot>,
    pub(crate) ranges: Vec<SegmentRangeRef>,
}

impl PreparedSnapshotDownload {
    pub fn requests(&self) -> Vec<crate::remote::DownloadRequest> {
        self.ranges.iter().map(|frame| crate::remote::DownloadRequest {
                path: format!("segments/{}", frame.sid.serialize()),
            range: Some(frame.bytes.clone()),
            length: frame.size(),
        }).collect()
    }
}

pub(crate) fn coalesce_ranges(ranges: Vec<SegmentRangeRef>, maximum: Option<usize>) -> Vec<SegmentRangeRef> {
    ranges.into_iter().coalesce(|a, b| {
        if maximum.is_some_and(|maximum| {
            a.size().checked_add(b.size()).is_none_or(|size| size > maximum as u64)
                || (u64::from(a.pageset.cardinality().to_u32())
                    + u64::from(b.pageset.cardinality().to_u32()))
                    > maximum as u64 / crate::core::page::PAGESIZE.as_u64()
        }) { Err((a, b)) } else { a.coalesce(b) }
    }).collect()
}

impl Action for PreparedSnapshotDownload {
    async fn run(self, storage: Arc<FjallStorage>, remote: Arc<Remote>) -> Result<(), GraftErr> {
        futures::stream::iter(self.ranges).map(Ok)
            .try_for_each_concurrent(HYDRATE_CONCURRENCY, |range| {
                FetchSegment { range }.run(storage.clone(), remote.clone())
            }).await?;
        for snapshot in self.snapshots {
            storage.mark_snapshot_hydrated(&snapshot)?;
        }
        Ok(())
    }
}

/// Downloads all missing pages for a Snapshot.
#[derive(Debug)]
pub struct HydrateSnapshot {
    pub snapshot: Snapshot,
}

impl Action for HydrateSnapshot {
    async fn run(self, storage: Arc<FjallStorage>, remote: Arc<Remote>) -> Result<(), GraftErr> {
        if storage.snapshot_hydration_cached(&self.snapshot)? {
            return Ok(());
        }
        let missing_frames = storage.read().find_missing_frames(&self.snapshot)?;
        futures::stream::iter(coalesce_ranges(missing_frames, remote.buffered_read_limit()))
        .map(Ok)
        .try_for_each_concurrent(HYDRATE_CONCURRENCY, |range| {
            FetchSegment { range }.run(storage.clone(), remote.clone())
        })
        .await?;
        storage.mark_snapshot_hydrated(&self.snapshot)?;
        Ok(())
    }
}
