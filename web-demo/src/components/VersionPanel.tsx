import { type FormEvent, useEffect, useRef, useState } from "react";
import { type Translate, useI18n } from "../i18n";
import { historyChanges } from "../lib/history";
import type {
  BranchInfo,
  CommitInfo,
  RepoStatus,
  RepoStatusPath,
  ResetMode,
} from "../types";

export type VersionTab = "changes" | "history";
export type ChangeSection = "staged" | "unstaged";

interface VersionPanelProps {
  activeTab: VersionTab;
  branches: BranchInfo[];
  busy: boolean;
  history: CommitInfo[];
  onCommit: (message: string) => Promise<void>;
  onCreateBranch: (name: string) => Promise<void>;
  onDiscardPaths: (paths: string[]) => Promise<boolean>;
  onMergeBranch: (branch: string) => Promise<void>;
  onReset: (target: string, mode: ResetMode) => Promise<boolean>;
  onTabChange: (tab: VersionTab) => void;
  onSelectHistory: (commit: CommitInfo, path?: string) => void;
  onSelectPath: (path: string, section: ChangeSection) => void;
  onStageAll: () => Promise<void>;
  onStagePath: (path: string) => Promise<void>;
  onSwitchBranch: (branch: string) => Promise<void>;
  onUnstageAll: () => Promise<void>;
  onUnstagePath: (path: string) => Promise<void>;
  selectedHistoryId?: string;
  selectedPath?: string;
  selectedSection?: ChangeSection;
  status: RepoStatus;
}

function shortId(id: string | undefined, unborn: string) {
  return id ? id.slice(0, 8) : unborn;
}

function formatDate(timestamp: number, locale: string) {
  return new Intl.DateTimeFormat(locale, {
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    month: "short",
  }).format(timestamp);
}

function kindLabel(kind: RepoStatusPath["kind"], t: Translate) {
  if (kind === "sqlite_database") return t("version.kind.sqlite_database");
  if (kind === "binary_file") return t("version.kind.binary_file");
  return t("version.kind.text_file");
}

function statusLabel(status: string | undefined, t: Translate) {
  if (status === "untracked") return t("version.change.untracked");
  if (status === "added") return t("version.change.added");
  if (status === "deleted") return t("version.change.deleted");
  if (status === "conflicted") return t("version.change.conflicted");
  return t("version.change.modified");
}

export function VersionPanel({
  activeTab,
  branches,
  busy,
  history,
  onCommit,
  onCreateBranch,
  onDiscardPaths,
  onMergeBranch,
  onReset,
  onTabChange,
  onSelectHistory,
  onSelectPath,
  onStageAll,
  onStagePath,
  onSwitchBranch,
  onUnstageAll,
  onUnstagePath,
  selectedHistoryId,
  selectedPath,
  selectedSection,
  status,
}: VersionPanelProps) {
  const { t } = useI18n();
  const [commitMessage, setCommitMessage] = useState("");
  const [branchName, setBranchName] = useState("");
  const [branchComposerOpen, setBranchComposerOpen] = useState(false);
  const [mergeComposerOpen, setMergeComposerOpen] = useState(false);
  const [mergeTarget, setMergeTarget] = useState("");
  const [discardRequest, setDiscardRequest] = useState<RepoStatusPath[] | null>(null);
  const [resetTarget, setResetTarget] = useState<CommitInfo | null>(null);

  async function submitCommit(event: FormEvent) {
    event.preventDefault();
    if (!commitMessage.trim()) return;
    await onCommit(commitMessage.trim());
    setCommitMessage("");
  }

  async function submitBranch(event: FormEvent) {
    event.preventDefault();
    if (!branchName.trim()) return;
    await onCreateBranch(branchName.trim());
    setBranchName("");
    setBranchComposerOpen(false);
  }

  async function submitMerge(event: FormEvent) {
    event.preventDefault();
    if (!mergeTarget) return;
    await onMergeBranch(mergeTarget);
    setMergeComposerOpen(false);
    setMergeTarget("");
  }

  const mergeCandidates = branches.filter((branch) => !branch.current && branch.target);

  return (
    <>
        <div className="version-panel">
          <div className="branch-bar">
            <label>
              <span className="sr-only">{t("version.currentBranch")}</span>
              <select
                disabled={busy || status.work_in_progress}
                onChange={(event) => void onSwitchBranch(event.target.value)}
                value={status.current_branch ?? ""}
              >
                {branches.map((branch) => (
                  <option key={branch.name} value={branch.name}>
                    {branch.name}
                  </option>
                ))}
              </select>
            </label>
            <button
              aria-expanded={branchComposerOpen}
              className="branch-create-toggle"
              disabled={busy || status.work_in_progress || !status.current_head}
              onClick={() => setBranchComposerOpen((current) => !current)}
              title={t(
                status.current_head
                  ? branchComposerOpen
                    ? "version.cancelBranch"
                    : "version.newBranchAction"
                  : "version.newBranchRequiresCommit",
              )}
              type="button"
            >
              <span aria-hidden="true">{branchComposerOpen ? "×" : "+"}</span>
              <span>
                {t(branchComposerOpen ? "version.cancelBranch" : "version.newBranchAction")}
              </span>
            </button>
            <button
              aria-expanded={mergeComposerOpen}
              className="branch-merge-toggle"
              disabled={busy || status.work_in_progress || mergeCandidates.length === 0}
              onClick={() => {
                setMergeComposerOpen((current) => !current);
                setBranchComposerOpen(false);
                setMergeTarget((current) => current || mergeCandidates[0]?.name || "");
              }}
              title={t("version.mergeBranch")}
              type="button"
            >
              <span aria-hidden="true">⇢</span>
              <span>{t("version.merge")}</span>
            </button>
          </div>

          {branchComposerOpen && (
            <form className="branch-create" onSubmit={submitBranch}>
              <input
                aria-label={t("version.newBranch")}
                autoFocus
                disabled={busy || status.work_in_progress || !status.current_head}
                onChange={(event) => setBranchName(event.target.value)}
                placeholder={t("version.newBranchPlaceholder")}
                value={branchName}
              />
              <button disabled={!branchName.trim() || busy} type="submit">
                {t("version.create")}
              </button>
            </form>
          )}

          {mergeComposerOpen && (
            <form className="branch-merge" onSubmit={submitMerge}>
              <label>
                <span>{t("version.mergeInto", { branch: status.current_branch ?? "HEAD" })}</span>
                <select
                  aria-label={t("version.mergeBranch")}
                  disabled={busy || status.work_in_progress}
                  onChange={(event) => setMergeTarget(event.target.value)}
                  value={mergeTarget}
                >
                  {mergeCandidates.map((branch) => (
                    <option key={branch.name} value={branch.name}>
                      {branch.name}
                    </option>
                  ))}
                </select>
              </label>
              <button disabled={busy || !mergeTarget} type="submit">
                {t("version.merge")}
              </button>
            </form>
          )}

          {status.merge_head && (
            <div className={`merge-state-banner ${status.has_conflicts ? "has-conflicts" : "is-ready"}`}>
              <span>{status.has_conflicts ? "!" : "✓"}</span>
              <div>
                <strong>
                  {t(status.has_conflicts ? "version.mergeConflicts" : "version.mergeReady")}
                </strong>
                <small>
                  {t("version.mergeTarget", { target: status.merge_head.slice(0, 8) })}
                </small>
              </div>
            </div>
          )}

          <div className="segmented-control" role="tablist">
            <button
              aria-selected={activeTab === "changes"}
              onClick={() => onTabChange("changes")}
              role="tab"
              type="button"
            >
              {t("version.changes")} <span>{status.paths.length}</span>
            </button>
            <button
              aria-selected={activeTab === "history"}
              onClick={() => onTabChange("history")}
              role="tab"
              type="button"
            >
              {t("version.history")} <span>{history.length}</span>
            </button>
          </div>

          <div className="version-list">
            {activeTab === "changes" ? (
              <ChangesList
                busy={busy}
                onRequestDiscard={setDiscardRequest}
                onSelectPath={onSelectPath}
                onStageAll={onStageAll}
                onStagePath={onStagePath}
                onUnstageAll={onUnstageAll}
                onUnstagePath={onUnstagePath}
                selectedPath={selectedPath}
                selectedSection={selectedSection}
                status={status}
              />
            ) : (
              <HistoryList
                branch={status.current_branch}
                currentHead={status.current_head}
                history={history}
                onRequestReset={setResetTarget}
                onSelect={onSelectHistory}
                selectedId={selectedHistoryId}
              />
            )}
          </div>

          {activeTab === "changes" && status.has_staged_changes && (
            <form className="commit-form" onSubmit={submitCommit}>
              <input
                aria-label={t("version.commitMessage")}
                disabled={busy}
                onChange={(event) => setCommitMessage(event.target.value)}
                placeholder={t("version.commitMessage")}
                value={commitMessage}
              />
              <button disabled={!commitMessage.trim() || busy} type="submit">
                {t("version.commit")}
              </button>
            </form>
          )}
        </div>

      {discardRequest && (
        <DiscardDialog
          busy={busy}
          changes={discardRequest}
          onCancel={() => setDiscardRequest(null)}
          onConfirm={async () => {
            if (await onDiscardPaths(discardRequest.map((change) => change.path))) {
              setDiscardRequest(null);
            }
          }}
        />
      )}

      {resetTarget && (
        <VersionResetDialog
          branch={status.current_branch}
          busy={busy}
          onCancel={() => setResetTarget(null)}
          onConfirm={async (mode) => {
            if (await onReset(resetTarget.id, mode)) setResetTarget(null);
          }}
          status={status}
          target={resetTarget}
        />
      )}
    </>
  );
}

interface ChangesListProps {
  busy: boolean;
  onRequestDiscard: (changes: RepoStatusPath[]) => void;
  onSelectPath: (path: string, section: ChangeSection) => void;
  onStageAll: () => Promise<void>;
  onStagePath: (path: string) => Promise<void>;
  onUnstageAll: () => Promise<void>;
  onUnstagePath: (path: string) => Promise<void>;
  selectedPath?: string;
  selectedSection?: ChangeSection;
  status: RepoStatus;
}

function ChangesList({
  busy,
  onRequestDiscard,
  onSelectPath,
  onStageAll,
  onStagePath,
  onUnstageAll,
  onUnstagePath,
  selectedPath,
  selectedSection,
  status,
}: ChangesListProps) {
  const { t } = useI18n();
  const stagedPaths = status.paths.filter(
    (path) => path.index_status !== "none" || path.staged_change !== undefined,
  );
  const unstagedPaths = status.paths.filter(
    (path) => path.worktree_status !== "none" || path.conflicted,
  );
  const discardablePaths = unstagedPaths.filter((path) => !path.conflicted);
  if (status.paths.length === 0) {
    return <div className="empty-list">{t("version.clean")}</div>;
  }

  function renderPath(path: RepoStatusPath, section: "staged" | "unstaged") {
    const staged = section === "staged";
    const canStage = !staged && !path.conflicted;
    const canDiscard = !staged && !path.conflicted;
    const change = staged
      ? (path.staged_change ?? "modified")
      : path.conflicted
        ? "conflicted"
        : (path.unstaged_change ?? "modified");
    return (
      <div
        className={`change-row ${selectedPath === path.path && selectedSection === section ? "is-selected" : ""}`}
        key={`${section}:${path.path}`}
      >
        <button
          className="change-main"
          onClick={() => onSelectPath(path.path, section)}
          type="button"
        >
          <span className={`change-code change-${change}`}>
            {change === "conflicted" ? "!" : change.slice(0, 1).toUpperCase()}
          </span>
          <span>
            <strong>{path.path}</strong>
            <small>
              <span>{kindLabel(path.kind, t)}</span>
              <i aria-hidden="true">·</i>
              <span>{statusLabel(change, t)}</span>
            </small>
          </span>
        </button>
        {!path.conflicted && <div className="change-row-actions">
          <button
            aria-label={t(canStage ? "version.stageAria" : "version.unstageAria", {
              path: path.path,
            })}
            className="change-action"
            disabled={busy}
            onClick={() => void (canStage ? onStagePath(path.path) : onUnstagePath(path.path))}
            title={t(canStage ? "version.stagePath" : "version.unstagePath")}
            type="button"
          >
            {canStage ? "+" : "−"}
          </button>
          {canDiscard && (
            <button
              aria-label={t("version.discardAria", { path: path.path })}
              className="change-action is-discard"
              disabled={busy}
              onClick={() => onRequestDiscard([path])}
              title={t("version.discardPath")}
              type="button"
            >
              ↶
            </button>
          )}
        </div>}
      </div>
    );
  }

  return (
    <div className="changes-sections">
      {stagedPaths.length > 0 && (
        <section className="changes-section" aria-labelledby="staged-changes-title">
          <header className="changes-section-header">
            <strong id="staged-changes-title">
              {t("version.stagedChanges")} <span>{stagedPaths.length}</span>
            </strong>
            <button
              disabled={busy}
              onClick={() => void onUnstageAll()}
              title={t("version.unstageAll")}
              type="button"
            >
              {t("version.unstageAll")}
            </button>
          </header>
          <div className="changes-section-list">
            {stagedPaths.map((path) => renderPath(path, "staged"))}
          </div>
        </section>
      )}

      {unstagedPaths.length > 0 && (
        <section className="changes-section" aria-labelledby="unstaged-changes-title">
          <header className="changes-section-header">
            <strong id="unstaged-changes-title">
              {t("version.unstagedChanges")} <span>{unstagedPaths.length}</span>
            </strong>
            <div>
              <button
                disabled={busy}
                onClick={() => void onStageAll()}
                title={t("version.stageAll")}
                type="button"
              >
                {t("version.stageAll")}
              </button>
              {discardablePaths.length > 0 && (
                <button
                  className="is-danger"
                  disabled={busy}
                  onClick={() => onRequestDiscard(discardablePaths)}
                  title={t("version.discardAll")}
                  type="button"
                >
                  {t("version.discardAll")}
                </button>
              )}
            </div>
          </header>
          <div className="changes-section-list">
            {unstagedPaths.map((path) => renderPath(path, "unstaged"))}
          </div>
        </section>
      )}

      {stagedPaths.length === 0 && unstagedPaths.length === 0 && (
        <section className="changes-section" aria-labelledby="conflicted-changes-title">
          <header className="changes-section-header">
            <strong id="conflicted-changes-title">
              {t("version.unstagedChanges")} <span>{status.paths.length}</span>
            </strong>
          </header>
          <div className="changes-section-list">
            {status.paths.map((path) => (
              <div
                className={`change-row ${selectedPath === path.path && selectedSection === "unstaged" ? "is-selected" : ""}`}
                key={`conflict:${path.path}`}
              >
                <button
                  className="change-main"
                  onClick={() => onSelectPath(path.path, "unstaged")}
                  type="button"
                >
                  <span className="change-code change-conflict">!</span>
                  <span>
                    <strong>{path.path}</strong>
                    <small>{statusLabel("conflicted", t)}</small>
                  </span>
                </button>
              </div>
            ))}
          </div>
        </section>
      )}
    </div>
  );
}

function HistoryList({
  branch,
  currentHead,
  history,
  onRequestReset,
  onSelect,
  selectedId,
}: {
  branch?: string;
  currentHead?: string;
  history: CommitInfo[];
  onRequestReset: (commit: CommitInfo) => void;
  onSelect: (commit: CommitInfo) => void;
  selectedId?: string;
}) {
  const { locale, t } = useI18n();
  if (history.length === 0) return <div className="empty-list">{t("version.noCommits")}</div>;
  return (
    <div className="history-list">
      {history.map((commit, index) => {
        const selected = selectedId === commit.id;
        const changes = historyChanges(commit);
        return (
          <div className="history-entry" key={commit.id}>
            <button
              className={selected ? "is-selected" : ""}
              onClick={() => onSelect(commit)}
              type="button"
            >
              <span className="history-rail" aria-hidden="true">
                <i />
                {index < history.length - 1 && <b />}
              </span>
              <span>
                <strong>{commit.message}</strong>
                <small>
                  {shortId(commit.id, t("version.unborn"))} ·{" "}
                  {formatDate(commit.timestamp_ms, locale)} · {changes.length}{" "}
                  {t("version.changes").toLowerCase()}
                </small>
              </span>
            </button>
            {selected && commit.id !== currentHead && (
              <div className="history-reset-action">
                <span>{t("version.moveHere", { branch: branch ?? "HEAD" })}</span>
                <button onClick={() => onRequestReset(commit)} type="button">
                  {t("version.reset")}
                </button>
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}

function DiscardDialog({
  busy,
  changes,
  onCancel,
  onConfirm,
}: {
  busy: boolean;
  changes: RepoStatusPath[];
  onCancel: () => void;
  onConfirm: () => Promise<void>;
}) {
  const { t } = useI18n();
  const dialogRef = useRef<HTMLDialogElement>(null);
  const cancelRef = useRef<HTMLButtonElement>(null);
  const untrackedCount = changes.filter(
    (change) => change.unstaged_change === "untracked",
  ).length;
  const trackedCount = changes.length - untrackedCount;

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    dialog.showModal();
    cancelRef.current?.focus();
    return () => {
      if (dialog.open) dialog.close();
    };
  }, []);

  return (
    <dialog
      aria-labelledby="discard-title"
      className="version-action-dialog"
      onCancel={(event) => {
        event.preventDefault();
        if (!busy) onCancel();
      }}
      ref={dialogRef}
    >
      <span>{t("discard.eyebrow")}</span>
      <h2 id="discard-title">
        {changes.length === 1
          ? t("discard.oneTitle", { path: changes[0].path })
          : t("discard.manyTitle", { count: changes.length })}
      </h2>
      {trackedCount > 0 && (
        <p>{t("discard.trackedBody")}</p>
      )}
      {untrackedCount > 0 && (
        <p className="version-dialog-warning">
          {t("discard.untrackedWarning", {
            count: untrackedCount,
            pathLabel: t(
              untrackedCount === 1 ? "discard.pathSingular" : "discard.pathPlural",
            ),
          })}
        </p>
      )}
      <ul className="version-dialog-paths">
        {changes.slice(0, 5).map((change) => (
          <li key={change.path}>
            <code>{change.path}</code>
            <span>{statusLabel(change.unstaged_change, t)}</span>
          </li>
        ))}
        {changes.length > 5 && (
          <li>{t("discard.morePaths", { count: changes.length - 5 })}</li>
        )}
      </ul>
      <div className="version-dialog-footer">
        <button disabled={busy} onClick={onCancel} ref={cancelRef} type="button">
          {t("discard.cancel")}
        </button>
        <button
          className="is-danger"
          disabled={busy}
          onClick={() => void onConfirm()}
          type="button"
        >
          {busy ? t("discard.busy") : t("discard.confirm")}
        </button>
      </div>
    </dialog>
  );
}

function VersionResetDialog({
  branch,
  busy,
  onCancel,
  onConfirm,
  status,
  target,
}: {
  branch?: string;
  busy: boolean;
  onCancel: () => void;
  onConfirm: (mode: ResetMode) => Promise<void>;
  status: RepoStatus;
  target: CommitInfo;
}) {
  const { t } = useI18n();
  const [mode, setMode] = useState<ResetMode>("soft");
  const dialogRef = useRef<HTMLDialogElement>(null);
  const cancelRef = useRef<HTMLButtonElement>(null);
  const resetModes: Array<{
    description: string;
    label: string;
    mode: ResetMode;
  }> = [
    {
      description: t("resetVersion.softDescription"),
      label: t("resetVersion.soft"),
      mode: "soft",
    },
    {
      description: t("resetVersion.mixedDescription"),
      label: t("resetVersion.mixed"),
      mode: "mixed",
    },
    {
      description: t("resetVersion.hardDescription"),
      label: t("resetVersion.hard"),
      mode: "hard",
    },
  ];

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    dialog.showModal();
    cancelRef.current?.focus();
    return () => {
      if (dialog.open) dialog.close();
    };
  }, []);

  return (
    <dialog
      aria-labelledby="version-reset-title"
      className="version-action-dialog"
      onCancel={(event) => {
        event.preventDefault();
        if (!busy) onCancel();
      }}
      ref={dialogRef}
    >
      <span>{t("resetVersion.eyebrow")}</span>
      <h2 id="version-reset-title">
        {t("resetVersion.title", {
          branch: branch ?? "HEAD",
          target: shortId(target.id, t("version.unborn")),
        })}
      </h2>
      <p>{t("resetVersion.body", { message: target.message })}</p>
      <fieldset className="reset-mode-options">
        <legend>{t("resetVersion.mode")}</legend>
        {resetModes.map((option) => (
          <label className={mode === option.mode ? "is-selected" : ""} key={option.mode}>
            <input
              checked={mode === option.mode}
              disabled={busy}
              name="reset-mode"
              onChange={() => setMode(option.mode)}
              type="radio"
              value={option.mode}
            />
            <span>
              <strong>{option.label}</strong>
              <code>--{option.mode}</code>
              <small>{option.description}</small>
            </span>
          </label>
        ))}
      </fieldset>
      {mode === "mixed" && status.has_staged_changes && (
        <p className="version-dialog-warning">
          {t("resetVersion.mixedWarning", {
            count: status.counts.staged,
            pathLabel: t(
              status.counts.staged === 1 ? "discard.pathSingular" : "discard.pathPlural",
            ),
          })}
        </p>
      )}
      {mode === "hard" && status.work_in_progress && (
        <p className="version-dialog-warning is-critical">
          {t("resetVersion.hardWarning", {
            changeLabel: t(
              status.paths.length === 1
                ? "resetVersion.changeSingular"
                : "resetVersion.changePlural",
            ),
            count: status.paths.length,
          })}
        </p>
      )}
      <div className="version-dialog-footer">
        <button disabled={busy} onClick={onCancel} ref={cancelRef} type="button">
          {t("resetVersion.cancel")}
        </button>
        <button
          className={mode === "hard" ? "is-danger" : ""}
          disabled={busy}
          onClick={() => void onConfirm(mode)}
          type="button"
        >
          {busy ? t("resetVersion.busy") : t("resetVersion.confirm", { mode })}
        </button>
      </div>
    </dialog>
  );
}
