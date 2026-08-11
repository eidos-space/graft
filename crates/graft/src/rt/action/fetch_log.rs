use std::{collections::HashSet, sync::Arc};

use crate::core::{
    LogId,
    lsn::{LSN, LSNRangeExt},
};
use range_set_blaze::RangeOnce;
use tokio_stream::StreamExt;

use crate::{
    local::fjall_storage::FjallStorage,
    remote::Remote,
    rt::action::{Action, Result},
};

/// Fetches new commits and metadata from a remote.
#[derive(Debug)]
pub struct FetchLog {
    pub log: LogId,
    /// Optional lower bound for repairing a known snapshot range.
    pub min_lsn: Option<LSN>,
    pub max_lsn: Option<LSN>,
}

impl Action for FetchLog {
    async fn run(self, storage: Arc<FjallStorage>, remote: Arc<Remote>) -> Result<()> {
        let reader = storage.read();
        let mut batch = storage.batch();

        // calculate the lsn range to retrieve
        let start = match self.min_lsn {
            Some(min_lsn) => min_lsn,
            None => reader
                .latest_lsn(&self.log)?
                .map_or(LSN::FIRST, |lsn| lsn.next()),
        };
        let end = self.max_lsn.unwrap_or(LSN::LAST);
        let lsns = start..=end;

        tracing::debug!(log = ?self.log, lsns = %lsns.to_string(), "fetching log");

        // Snapshot hydration supplies an exact lower bound and must repair holes even when the
        // derived LSN index says a commit exists. Unbounded sync can keep using the indexed tail.
        let missing_lsns: Box<dyn Iterator<Item = LSN> + Send> = if self.min_lsn.is_some() {
            let mut missing = Vec::new();
            for lsn in lsns.iter() {
                if reader.get_commit(&self.log, lsn)?.is_none() {
                    missing.push(lsn);
                }
            }
            Box::new(missing.into_iter())
        } else {
            let existing_lsns = storage.read().lsns(&self.log, &lsns)?;
            Box::new(
                (RangeOnce::new(lsns) - existing_lsns.into_ranges())
                    .flat_map(|range| range.iter()),
            )
        };

        let mut seen_lsns = HashSet::new();
        let mut checkpoints = HashSet::new();

        // fetch missing lsns
        let mut commits = remote.stream_commits_ordered(&self.log, missing_lsns);
        while let Some(commit) = commits.try_next().await? {
            seen_lsns.insert(commit.lsn);
            // keep track of checkpoints that we need to re-fetch
            checkpoints.extend(
                commit
                    .checkpoints
                    .iter()
                    .copied()
                    .filter(|lsn| !seen_lsns.contains(lsn)),
            );
            batch.write_commit(commit);
        }

        // fetch missing checkpoints
        if !checkpoints.is_empty() {
            let mut commits = remote.stream_commits_ordered(&self.log, checkpoints);
            while let Some(commit) = commits.try_next().await? {
                batch.write_commit(commit);
            }
        }

        Ok(batch.commit()?)
    }
}
