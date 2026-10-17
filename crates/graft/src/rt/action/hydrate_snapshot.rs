use std::sync::Arc;

use futures::{StreamExt, TryStreamExt};
use itertools::Itertools;

use crate::{
    GraftErr,
    local::fjall_storage::FjallStorage,
    remote::Remote,
    rt::action::{Action, fetch_segment::FetchSegment},
    snapshot::Snapshot,
};

const HYDRATE_CONCURRENCY: usize = 5;

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
        let maximum = remote.buffered_read_limit();
        futures::stream::iter(
            missing_frames
                .into_iter()
                // Preserve frame boundaries when a larger request would exceed
                // the host's encrypted read budget. Also bound decompressed
                // batches for highly compressible databases.
                .coalesce(|a, b| {
                    if maximum.is_some_and(|maximum| {
                        a.size()
                            .checked_add(b.size())
                            .is_none_or(|size| size > maximum as u64)
                            || (u64::from(a.pageset.cardinality().to_u32())
                                + u64::from(b.pageset.cardinality().to_u32()))
                                > maximum as u64 / crate::core::page::PAGESIZE.as_u64()
                    }) {
                        Err((a, b))
                    } else {
                        a.coalesce(b)
                    }
                }),
        )
        .map(Ok)
        .try_for_each_concurrent(HYDRATE_CONCURRENCY, |range| {
            FetchSegment { range }.run(storage.clone(), remote.clone())
        })
        .await?;
        storage.mark_snapshot_hydrated(&self.snapshot)?;
        Ok(())
    }
}
