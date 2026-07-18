import { useI18n, type Translate } from "../i18n";
import { historyChanges } from "../lib/history";
import type { CommitChange, CommitInfo } from "../types";

interface CommitFilesPaneProps {
  commit?: CommitInfo;
  onSelectPath: (commit: CommitInfo, path: string) => void;
  selectedPath?: string;
}

function kindLabel(kind: CommitChange["kind"], t: Translate) {
  if (kind === "sqlite_database") return t("version.kind.sqlite_database");
  if (kind === "binary_file") return t("version.kind.binary_file");
  return t("version.kind.text_file");
}

function changeLabel(change: CommitChange["change"], t: Translate) {
  if (change === "added") return t("version.change.added");
  if (change === "deleted") return t("version.change.deleted");
  return t("version.change.modified");
}

function formatDate(timestamp: number, locale: string) {
  return new Intl.DateTimeFormat(locale, {
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    month: "short",
    year: "numeric",
  }).format(timestamp);
}

export function CommitFilesPane({
  commit,
  onSelectPath,
  selectedPath,
}: CommitFilesPaneProps) {
  const { locale, t } = useI18n();
  const changes = commit ? historyChanges(commit) : [];

  return (
    <section className="commit-files-pane" data-area="commit-files">
      {commit ? (
        <>
          <div className="commit-summary">
            <div className="commit-summary-meta">
              <span>{t("version.commitDetails")}</span>
              <code>{commit.id.slice(0, 8)}</code>
            </div>
            <strong>{commit.message}</strong>
            <small>
              {formatDate(commit.timestamp_ms, locale)}
              {commit.parent ? ` · ${commit.parent.slice(0, 8)} → ${commit.id.slice(0, 8)}` : ""}
            </small>
          </div>
          <div className="commit-files-group">
            <div className="commit-files-heading">
              <span>{t("version.commitFiles")}</span>
              <b>{changes.length}</b>
            </div>
            <div className="commit-file-list">
              {changes.length > 0 ? (
                changes.map((change) => {
                  const active = selectedPath === change.path;
                  return (
                    <button
                      aria-current={active ? "page" : undefined}
                      aria-label={t("version.openCommitFile", {
                        commit: commit.id.slice(0, 8),
                        path: change.path,
                      })}
                      className={active ? "is-selected" : ""}
                      key={change.path}
                      onClick={() => onSelectPath(commit, change.path)}
                      title={change.path}
                      type="button"
                    >
                      <span className={`change-code change-${change.change}`}>
                        {change.change.slice(0, 1).toUpperCase()}
                      </span>
                      <strong>{change.path}</strong>
                      <small>
                        {kindLabel(change.kind, t)} · {changeLabel(change.change, t)}
                      </small>
                    </button>
                  );
                })
              ) : (
                <div className="commit-files-empty">{t("version.noCommitFiles")}</div>
              )}
            </div>
          </div>
        </>
      ) : (
        <div className="commit-files-placeholder">
          <span>{t("version.commitDetails")}</span>
          <strong>{t("version.selectCommit")}</strong>
          <p>{t("version.selectCommitBody")}</p>
        </div>
      )}
    </section>
  );
}
