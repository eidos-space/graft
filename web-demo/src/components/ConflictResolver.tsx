import { type FormEvent, useEffect, useMemo, useState } from "react";
import { useI18n } from "../i18n";
import type {
  ConflictResolution,
  MergeContent,
  MergePath,
  MergeStatus,
  MergeVersion,
  RepoConflictArtifact,
  RepoConflictList,
  RepoConflictPath,
} from "../types";

export interface ConflictResolutionRequest {
  path: string;
  resolution: Exclude<ConflictResolution, "manual">;
  row?: {
    identity: number | Record<string, unknown>;
    table: string;
  };
}

interface ConflictResolverProps {
  busy: boolean;
  conflicts: RepoConflictList;
  mergePaths?: MergePath[];
  mergeStatus?: MergeStatus;
  onAbort: () => Promise<boolean>;
  onContinue: (message: string) => Promise<boolean>;
  onReadVersion?: (path: string, version: MergeVersion) => Promise<MergeContent | undefined>;
  onResolve: (request: ConflictResolutionRequest) => Promise<boolean>;
  onSelectPath: (path: string) => void;
  onWriteTextResult?: (path: string, content: string) => Promise<boolean>;
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

function kindLabel(path: RepoConflictPath | MergePath, t: ReturnType<typeof useI18n>["t"]) {
  if (path.kind === "sqlite_database") return t("version.kind.sqlite_database");
  if (path.kind === "binary_file") return t("version.kind.binary_file");
  return t("version.kind.text_file");
}

export function ConflictResolver({
  busy,
  conflicts,
  mergePaths = [],
  mergeStatus,
  onAbort,
  onContinue,
  onReadVersion,
  onResolve,
  onSelectPath,
  onWriteTextResult,
  selectedPath,
}: ConflictResolverProps) {
  const { t } = useI18n();
  const durableMerging = mergeStatus?.state === "merging";
  const pathItems: MergePath[] = mergePaths.length > 0
    ? mergePaths
    : conflicts.paths.map((item) => ({
        has_base: true,
        has_ours: true,
        has_theirs: true,
        kind: item.kind,
        path: item.path,
        state: item.unresolved > 0 ? "unmerged" : "resolved",
        storage: item.storage,
      }));
  const path = pathItems.find((candidate) => candidate.path === selectedPath) ?? pathItems[0];
  const artifacts = useMemo(
    () => conflicts.conflicts.filter((conflict) => conflict.path === path?.path),
    [conflicts.conflicts, path?.path],
  );
  const unresolved = durableMerging
    ? mergeStatus.unmerged_count
    : conflicts.paths.reduce((sum, item) => sum + item.unresolved, 0);
  const resolved = durableMerging
    ? pathItems.filter((item) => item.state === "resolved").length
    : conflicts.paths.reduce((sum, item) => sum + item.resolved, 0);
  const readyToContinue = durableMerging && unresolved === 0;
  const [message, setMessage] = useState("");

  async function submitMerge(event: FormEvent) {
    event.preventDefault();
    if (!message.trim()) return;
    if (await onContinue(message.trim())) setMessage("");
  }

  if (!durableMerging && !conflicts.merge_head) {
    return (
      <section className="conflict-workspace is-empty" aria-label={t("conflict.label")}>
        <span>{t("conflict.eyebrow")}</span>
        <h1>{t("conflict.noMerge")}</h1>
        <p>{t("conflict.noMergeBody")}</p>
      </section>
    );
  }

  const mergeHead = durableMerging ? mergeStatus.merge_head : conflicts.merge_head;

  return (
    <section className="conflict-workspace" aria-label={t("conflict.label")}>
      <header className="conflict-header">
        <div>
          <span>{t("conflict.eyebrow")}</span>
          <h1>{readyToContinue ? t("conflict.readyTitle") : t("conflict.title")}</h1>
          <p>
            {t("conflict.merging", {
              branch: conflicts.current_branch ?? "HEAD",
              target: shortId(mergeHead),
            })}
          </p>
        </div>
        <div className="conflict-progress" aria-live="polite">
          <strong>{unresolved}</strong>
          <span>{t("conflict.remaining")}</span>
          <small>{t("conflict.resolvedCount", { count: resolved })}</small>
          {durableMerging && (
            <code title={mergeStatus.state_token}>
              {t("conflict.stateToken", { token: shortId(mergeStatus.state_token) })}
            </code>
          )}
        </div>
      </header>

      <div className="conflict-lineage" aria-label={t("conflict.lineage")}>
        <span><strong>LOCAL</strong>{conflicts.current_branch ?? "HEAD"}</span>
        <i aria-hidden="true">→</i>
        <span><strong>HOSTED</strong>{t("conflict.hostedRevision", { revision: shortId(mergeHead) })}</span>
        <i aria-hidden="true">→</i>
        <span><strong>BASE</strong>{durableMerging ? shortId(mergeStatus.merge_base ?? undefined) : "—"}</span>
      </div>

      <div className="conflict-layout">
        <nav className="conflict-paths" aria-label={t("conflict.paths")}>
          <div className="conflict-paths-heading">
            <strong>{t("conflict.paths")}</strong>
            <span>{pathItems.length}</span>
          </div>
          {pathItems.map((item) => {
            const itemConflicts = conflicts.paths.find((candidate) => candidate.path === item.path);
            const itemUnresolved = itemConflicts?.unresolved ?? (item.state === "unmerged" ? 1 : 0);
            return (
              <button
                aria-current={item.path === path?.path ? "page" : undefined}
                disabled={busy}
                key={item.path}
                onClick={() => onSelectPath(item.path)}
                type="button"
              >
                <span className={item.state === "resolved" ? "is-resolved" : ""}>
                  {item.state === "resolved" ? "✓" : "!"}
                </span>
                <span>
                  <strong>{item.path}</strong>
                  <small>
                    {kindLabel(item, t)} · {t("conflict.unresolvedCount", { count: itemUnresolved })}
                  </small>
                </span>
              </button>
            );
          })}
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
                {path.state === "unmerged" && (
                  <div className="conflict-file-actions">
                    <button
                      disabled={busy}
                      onClick={() => void onResolve({ path: path.path, resolution: "ours" })}
                      type="button"
                    >
                      {t("conflict.useAllOurs")}
                    </button>
                    <button
                      disabled={busy}
                      onClick={() => void onResolve({ path: path.path, resolution: "theirs" })}
                      type="button"
                    >
                      {t("conflict.useAllTheirs")}
                    </button>
                  </div>
                )}
              </div>

              {path.kind === "text_file" && onReadVersion && onWriteTextResult ? (
                <TextConflictView
                  busy={busy}
                  onReadVersion={onReadVersion}
                  onWriteTextResult={onWriteTextResult}
                  path={path.path}
                  stateToken={durableMerging ? mergeStatus.state_token : undefined}
                />
              ) : null}

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

function TextConflictView({
  busy,
  onReadVersion,
  onWriteTextResult,
  path,
  stateToken,
}: {
  busy: boolean;
  onReadVersion: (path: string, version: MergeVersion) => Promise<MergeContent | undefined>;
  onWriteTextResult: (path: string, content: string) => Promise<boolean>;
  path: string;
  stateToken?: string;
}) {
  const { t } = useI18n();
  const [versions, setVersions] = useState<Partial<Record<MergeVersion, MergeContent>>>({});
  const [draft, setDraft] = useState("");
  const [loading, setLoading] = useState(true);
  useEffect(() => {
    let active = true;
    setLoading(true);
    void Promise.all(
      (["base", "ours", "theirs", "result"] as MergeVersion[]).map(async (version) => [
        version,
        await onReadVersion(path, version),
      ] as const),
    ).then((entries) => {
      if (!active) return;
      const next = Object.fromEntries(entries.filter(([, value]) => value)) as Partial<
        Record<MergeVersion, MergeContent>
      >;
      setVersions(next);
      const result = next.result?.content;
      setDraft(result?.state === "utf8" ? result.content : "");
      setLoading(false);
    });
    return () => {
      active = false;
    };
  }, [onReadVersion, path, stateToken]);

  function versionText(version: MergeVersion) {
    const content = versions[version]?.content;
    if (!content || content.state === "absent") return t("conflict.versionAbsent");
    if (content.state === "utf8") return content.content;
    return `[${content.state.replaceAll("_", " ")}]`;
  }

  return (
    <section className="conflict-text-review" aria-label={t("conflict.textReview")}>
      <header>
        <div>
          <span>{t("conflict.textEyebrow")}</span>
          <strong>{t("conflict.textTitle")}</strong>
        </div>
        {loading ? <small>{t("conflict.loadingVersions")}</small> : null}
      </header>
      <div className="conflict-version-grid">
        {(["base", "ours", "theirs"] as MergeVersion[]).map((version) => (
          <article key={version}>
            <header><strong>{version.toUpperCase()}</strong><small>{versionText(version).length} bytes</small></header>
            <pre>{versionText(version)}</pre>
          </article>
        ))}
      </div>
      <label className="conflict-result-editor">
        <span>{t("conflict.resultLabel")}</span>
        <textarea
          aria-label={t("conflict.resultLabel")}
          disabled={busy || loading}
          onChange={(event) => setDraft(event.target.value)}
          value={draft}
        />
      </label>
      <div className="conflict-result-footer">
        <small>{t("conflict.resultHelp")}</small>
        <button
          disabled={busy || loading}
          onClick={() => void onWriteTextResult(path, draft)}
          type="button"
        >
          {t("conflict.saveResult")}
        </button>
      </div>
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
  const isRow = artifact.kind === "row" && artifact.table;
  const resolved = artifact.status === "resolved";
  const identity = artifact.rowid ?? artifact.key ?? artifact.ours_key ?? artifact.theirs_key;

  return (
    <article className={`conflict-artifact is-${artifact.kind} ${resolved ? "is-resolved" : ""}`}>
      <header>
        <div>
          <span>{isRow ? t("conflict.rowConflict") : t("conflict.fileConflict")}</span>
          <strong>
            {isRow
              ? t("conflict.rowTitle", { rowid: artifact.rowid ?? "key", table: artifact.table ?? "—" })
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
              disabled={busy || resolved || identity === undefined}
              onClick={() =>
                void onResolve({
                  path: artifact.path,
                  resolution: "ours",
                  row: { identity: identity as number | Record<string, unknown>, table: artifact.table! },
                })
              }
              type="button"
            >
              {artifact.resolution === "ours" ? t("conflict.chosenOurs") : t("conflict.chooseOurs")}
            </button>
          </div>
          <div className="conflict-side-choice is-theirs">
            <RowState label={t("conflict.theirs")} row={artifact.theirs_row} />
            <button
              disabled={busy || resolved || identity === undefined}
              onClick={() =>
                void onResolve({
                  path: artifact.path,
                  resolution: "theirs",
                  row: { identity: identity as number | Record<string, unknown>, table: artifact.table! },
                })
              }
              type="button"
            >
              {artifact.resolution === "theirs" ? t("conflict.chosenTheirs") : t("conflict.chooseTheirs")}
            </button>
          </div>
        </div>
      )}
    </article>
  );
}
