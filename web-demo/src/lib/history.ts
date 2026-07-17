import type { CommitChange, CommitInfo } from "../types";

type CommitWithChanges = Pick<CommitInfo, "changes">;

export function historyChanges(commit: CommitWithChanges): CommitChange[] {
  return commit.changes ?? [];
}

export function historyChange(
  commit: CommitWithChanges,
  path?: string,
): CommitChange | undefined {
  const changes = historyChanges(commit);
  return path ? changes.find((change) => change.path === path) : changes[0];
}
