import { type FormEvent, useMemo, useState } from "react";
import { useI18n } from "../i18n";
import type {
  ConflictResolution,
  RepoConflictArtifact,
  RepoConflictList,
  RepoConflictPath,
} from "../types";

export interface ConflictResolutionRequest {
  path: string;
  resolution: ConflictResolution;
  row?: {
    rowid: number;
    table: string;
  };
}

interface ConflictResolverProps {
  busy: boolean;
  conflicts: RepoConflictList;
  onAbort: () => Promise<boolean>;
  onContinue: (message: string) => Promise<boolean>;
  onResolve: (request: ConflictResolutionRequest) => Promise<boolean>;
  onSelectPath: (path: string) => void;
  selectedPath?: string;
}

function shortId(value?: string) {
  return value ? value.slice(0, 8) : "—";
}

function readableReason(value: string) {
  return value.replaceAll("_", " ");
}

function displayValue(value: unknown) {
  if (value === null) return "NULL";
  if (typeof value === "string") return value;
  if (typeof value === "undefined") return "—";
  return JSON.stringify(value);
}

function RowState({ label, row }: { label: string; row?: unknown[] }) {
  const { t } = useI18n();
  return (
    <div className="conflict-row-state">
      <strong>{label}</strong>
      {row ? (
        <ol>
          {row.map((value, index) => (
            <li key={index}>
              <span>{index + 1}</span>
              <code>{displayValue(value)}</code>
            </li>
          ))}
        </ol>
      ) : (
        <p>{t("conflict.rowAbsent")}</p>
      )}
    </div>
  );
}

function kindLabel(path: RepoConflictPath, t: ReturnType<typeof useI18n>["t"]) {
  if (path.kind === "sqlite_database") return t("version.kind.sqlite_database");
  if (path.kind === "binary_file") return t("version.kind.binary_file");
  return t("version.kind.text_file");
}

export function ConflictResolver({
  busy,
  conflicts,
  onAbort,
  onContinue,
  onResolve,
  onSelectPath,
  selectedPath,
}: ConflictResolverProps) {
  const { t } = useI18n();
  const [message, setMessage] = useState("");
  const path =
    conflicts.paths.find((candidate) => candidate.path === selectedPath) ??
    conflicts.paths[0];
  const artifacts = useMemo(
    () => conflicts.conflicts.filter((conflict) => conflict.path === path?.path),
    [conflicts.conflicts, path?.path],
  );
  const unresolved = conflicts.paths.reduce((sum, item) => sum + item.unresolved, 0);
  const resolved = conflicts.paths.reduce((sum, item) => sum + item.resolved, 0);
  const readyToContinue = Boolean(conflicts.merge_head) && unresolved === 0;

  async function submitMerge(event: FormEvent) {
    event.preventDefault();
    if (!message.trim()) return;
    if (await onContinue(message.trim())) setMessage("");
  }

  if (!conflicts.merge_head) {
    return (
      <section className="conflict-workspace is-empty" aria-label={t("conflict.label")}>
        <span>{t("conflict.eyebrow")}</span>
        <h1>{t("conflict.noMerge")}</h1>
        <p>{t("conflict.noMergeBody")}</p>
      </section>
    );
  }

  return (
    <section className="conflict-workspace" aria-label={t("conflict.label")}>
      <header className="conflict-header">
        <div>
          <span>{t("conflict.eyebrow")}</span>
          <h1>{readyToContinue ? t("conflict.readyTitle") : t("conflict.title")}</h1>
          <p>
            {t("conflict.merging", {
              branch: conflicts.current_branch ?? "HEAD",
              target: shortId(conflicts.merge_head),
            })}
          </p>
        </div>
        <div className="conflict-progress" aria-live="polite">
          <strong>{unresolved}</strong>
          <span>{t("conflict.remaining")}</span>
          <small>{t("conflict.resolvedCount", { count: resolved })}</small>
        </div>
      </header>

      <div className="conflict-layout">
        <nav className="conflict-paths" aria-label={t("conflict.paths")}>
          <div className="conflict-paths-heading">
            <strong>{t("conflict.paths")}</strong>
            <span>{conflicts.paths.length}</span>
          </div>
          {conflicts.paths.map((item) => (
            <button
              aria-current={item.path === path?.path ? "page" : undefined}
              key={item.path}
              onClick={() => onSelectPath(item.path)}
              type="button"
            >
              <span className={item.unresolved === 0 ? "is-resolved" : ""}>
                {item.unresolved === 0 ? "✓" : "!"}
              </span>
              <span>
                <strong>{item.path}</strong>
                <small>
                  {kindLabel(item, t)} · {t("conflict.unresolvedCount", {
                    count: item.unresolved,
                  })}
                </small>
              </span>
            </button>
          ))}
          <button
            className="conflict-abort"
            disabled={busy}
            onClick={() => void onAbort()}
            type="button"
          >
            {t("conflict.abort")}
          </button>
        </nav>

        <div className="conflict-detail">
          {path ? (
            <>
              <div className="conflict-file-header">
                <div>
                  <span>{t("conflict.path")}</span>
                  <h2>{path.path}</h2>
                  <p>{t("conflict.sideHelp")}</p>
                </div>
                {path.unresolved > 0 && (
                  <div className="conflict-file-actions">
                    <button
                      disabled={busy}
                      onClick={() =>
                        void onResolve({ path: path.path, resolution: "ours" })
                      }
                      type="button"
                    >
                      {t("conflict.useAllOurs")}
                    </button>
                    <button
                      disabled={busy}
                      onClick={() =>
                        void onResolve({ path: path.path, resolution: "theirs" })
                      }
                      type="button"
                    >
                      {t("conflict.useAllTheirs")}
                    </button>
                    <button
                      className="is-secondary"
                      disabled={busy}
                      onClick={() =>
                        void onResolve({ path: path.path, resolution: "manual" })
                      }
                      title={t("conflict.manualHelp")}
                      type="button"
                    >
                      {t("conflict.markManual")}
                    </button>
                  </div>
                )}
              </div>

              <div className="conflict-artifacts">
                {artifacts.length === 0 ? (
                  <div className="conflict-resolved-note">
                    <strong>{t("conflict.pathResolved")}</strong>
                    <p>{t("conflict.pathResolvedBody")}</p>
                  </div>
                ) : (
                  artifacts.map((artifact) => (
                    <ConflictArtifactView
                      artifact={artifact}
                      busy={busy}
                      key={artifact.id}
                      onResolve={onResolve}
                    />
                  ))
                )}
              </div>
            </>
          ) : (
            <div className="conflict-resolved-note">
              <strong>{t("conflict.allResolved")}</strong>
              <p>{t("conflict.allResolvedBody")}</p>
            </div>
          )}
        </div>
      </div>

      {readyToContinue && (
        <form className="merge-continue-bar" onSubmit={submitMerge}>
          <div>
            <span>{t("conflict.readyEyebrow")}</span>
            <strong>{t("conflict.readyBody")}</strong>
          </div>
          <input
            aria-label={t("conflict.mergeMessage")}
            disabled={busy}
            onChange={(event) => setMessage(event.target.value)}
            placeholder={t("conflict.mergeMessage")}
            value={message}
          />
          <button disabled={busy || !message.trim()} type="submit">
            {busy ? t("conflict.finishing") : t("conflict.continue")}
          </button>
        </form>
      )}
    </section>
  );
}

function ConflictArtifactView({
  artifact,
  busy,
  onResolve,
}: {
  artifact: RepoConflictArtifact;
  busy: boolean;
  onResolve: (request: ConflictResolutionRequest) => Promise<boolean>;
}) {
  const { t } = useI18n();
  const isRow = artifact.kind === "row" && artifact.table && artifact.rowid !== undefined;
  const resolved = artifact.status === "resolved";

  return (
    <article className={`conflict-artifact is-${artifact.kind} ${resolved ? "is-resolved" : ""}`}>
      <header>
        <div>
          <span>{isRow ? t("conflict.rowConflict") : t("conflict.fileConflict")}</span>
          <strong>
            {isRow
              ? t("conflict.rowTitle", {
                  rowid: artifact.rowid ?? "—",
                  table: artifact.table ?? "—",
                })
              : (artifact.name ?? readableReason(artifact.reason))}
          </strong>
        </div>
        <span className={resolved ? "is-resolved" : ""}>
          {resolved ? t("conflict.resolved") : readableReason(artifact.reason)}
        </span>
      </header>

      {artifact.columns && artifact.columns.length > 0 && (
        <p className="conflict-columns">
          {t("conflict.columns")}: {artifact.columns.join(", ")}
        </p>
      )}
      {artifact.message && <p className="conflict-message">{artifact.message}</p>}

      {isRow && (
        <div className="conflict-row-comparison">
          <RowState label={t("conflict.base")} row={artifact.base_row} />
          <div className="conflict-side-choice">
            <RowState label={t("conflict.ours")} row={artifact.ours_row} />
            <button
              disabled={busy || resolved}
              onClick={() =>
                void onResolve({
                  path: artifact.path,
                  resolution: "ours",
                  row: { rowid: artifact.rowid!, table: artifact.table! },
                })
              }
              type="button"
            >
              {artifact.resolution === "ours"
                ? t("conflict.chosenOurs")
                : t("conflict.chooseOurs")}
            </button>
          </div>
          <div className="conflict-side-choice is-theirs">
            <RowState label={t("conflict.theirs")} row={artifact.theirs_row} />
            <button
              disabled={busy || resolved}
              onClick={() =>
                void onResolve({
                  path: artifact.path,
                  resolution: "theirs",
                  row: { rowid: artifact.rowid!, table: artifact.table! },
                })
              }
              type="button"
            >
              {artifact.resolution === "theirs"
                ? t("conflict.chosenTheirs")
                : t("conflict.chooseTheirs")}
            </button>
          </div>
        </div>
      )}
    </article>
  );
}
