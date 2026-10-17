import { lazy, Suspense, useState } from "react";
import { api, useResource } from "./api";
import type { Content, DatabaseCatalog } from "./types";
import { SegmentedTabs } from "./SegmentedTabs";
import { useId } from "react";
import { useReviewBusy } from "./useReviewBusy";
import type { DiffPreferences } from "./Code";
const Code = lazy(() => import("./Code").then((module) => ({ default: module.Code })));
const TextConflict = lazy(() =>
  import("./TextConflict").then((module) => ({ default: module.TextConflict })),
);
const DatabaseConflict = lazy(() =>
  import("./DatabaseConflict").then((module) => ({ default: module.DatabaseConflict })),
);
const Database = lazy(() => import("./Database").then((module) => ({ default: module.Database })));
const ignoreCatalog = () => {};

function ResolvedDatabase({ path }: { path: string }) {
  const id = useId();
  const catalog = useResource<DatabaseCatalog>("database-tables", { path, view: "file" });
  const [selected, setSelected] = useState("");
  const table =
    catalog.data?.tables.find((item) => item.name === selected)?.name ??
    catalog.data?.tables[0]?.name;
  return (
    <>
      <div className="conflict-actions">
        <span>Resolved · saved result</span>
      </div>
      {catalog.error ? (
        <div className="notice" role="alert">
          {catalog.error}
          <button onClick={catalog.retry}>Retry</button>
        </div>
      ) : !catalog.data ? (
        <div className="empty">Loading tables…</div>
      ) : table ? (
        <>
          <SegmentedTabs
            id={id}
            label="Resolved tables"
            panel={`${id}-table`}
            value={table}
            onChange={setSelected}
            options={catalog.data.tables.map((item) => ({ value: item.name, label: item.name }))}
          />
          <div
            className="table-diff-panel"
            id={`${id}-table`}
            role="tabpanel"
            aria-labelledby={`${id}-${table}`}
          >
            <Suspense fallback={<div className="empty">Loading database…</div>}>
              <Database
                path={path}
                table={table}
                compare={false}
                mode="files"
                onCatalog={ignoreCatalog}
              />
            </Suspense>
          </div>
        </>
      ) : (
        <div className="empty">This database has no tables.</div>
      )}
    </>
  );
}
export interface MergePage {
  status: {
    state: "none" | "merging";
    state_token?: string;
    unmerged_count?: number;
    orig_head?: string;
    merge_head?: string;
  };
  paths: { path: string; state: "unmerged" | "resolved"; kind: string }[];
  next_cursor?: string;
}
interface ConflictPage {
  base: { content: Content };
  ours: { content: Content };
  theirs: { content: Content };
  result: { content: Content };
  result_etag?: string;
  conflicts: Record<string, unknown>[];
  next_cursor?: string;
}
function valueText(value: unknown) {
  return value === undefined ? "—" : typeof value === "string" ? value : JSON.stringify(value);
}
function ConflictRecord({ conflict }: { conflict: Record<string, unknown> }) {
  const columns = Array.isArray(conflict.columns) ? conflict.columns.map(String) : [];
  const current = Array.isArray(conflict.ours_row) ? conflict.ours_row : [];
  const incoming = Array.isArray(conflict.theirs_row) ? conflict.theirs_row : [];
  const base = Array.isArray(conflict.base_row) ? conflict.base_row : [];
  const cells = Array.isArray(conflict.cells)
    ? conflict.cells.filter(
        (cell): cell is Record<string, unknown> => typeof cell === "object" && cell !== null,
      )
    : [];
  const rows = (
    cells.length
      ? cells.map((cell) => ({
          field: String(cell.column),
          base: cell.base,
          current: cell.ours,
          incoming: cell.theirs,
        }))
      : columns.map((field, index) => ({
          field,
          base: base[index],
          current: current[index],
          incoming: incoming[index],
        }))
  ).filter((row) => valueText(row.current) !== valueText(row.incoming));
  return (
    <section className="conflict-record">
      <strong>{String(conflict.table ?? conflict.name ?? "File conflict")}</strong>
      {conflict.rowid !== undefined && (
        <span className="conflict-row-key">Row {String(conflict.rowid)}</span>
      )}
      {conflict.key !== undefined && (
        <span className="conflict-row-key">Key {valueText(conflict.key)}</span>
      )}
      {rows.length ? (
        <table>
          <thead>
            <tr>
              <th>Field</th>
              <th>Base</th>
              <th>Current</th>
              <th>Incoming</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row, index) => (
              <tr key={index}>
                <th>{row.field}</th>
                <td>{valueText(row.base)}</td>
                <td>{valueText(row.current)}</td>
                <td>{valueText(row.incoming)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : (
        <p>
          {String(
            conflict.message ??
              "The branches contain conflicting changes to this file or its structure.",
          )}
        </p>
      )}
    </section>
  );
}
export function ConflictDetail({
  path,
  token,
  resolved,
  preferences,
  onChanged,
  draftScope,
  kind,
}: {
  path: string;
  token: string;
  resolved: boolean;
  preferences: DiffPreferences;
  onChanged: () => void;
  draftScope: string;
  kind?: string;
}) {
  const [after, setAfter] = useState<string>();
  const data = useResource<ConflictPage>(
    "conflict",
    {
      path,
      state_token: token,
      after,
    },
    false,
    false,
    kind !== "sqlite_database",
  );
  const [side, setSide] = useState<"ours" | "theirs">();
  const [busy, setBusy] = useState(false);
  useReviewBusy(busy);
  const [error, setError] = useState("");
  const resolve = async () => {
    setBusy(true);
    setError("");
    try {
      await api("resolve", { path, state_token: token, action: side });
      setSide(undefined);
      onChanged();
    } catch (error) {
      setError(error instanceof Error ? error.message : "Could not resolve conflict");
    } finally {
      setBusy(false);
    }
  };
  const text =
    data.data &&
    [data.data.ours.content, resolved ? data.data.result.content : data.data.theirs.content].every(
      (content) => content.state === "utf8" || content.state === "absent",
    );
  if (
    kind === "sqlite_database" ||
    data.data?.ours.content.state === "sqlite" ||
    data.data?.theirs.content.state === "sqlite"
  ) {
    if (resolved) return <ResolvedDatabase path={path} />;
    return (
      <Suspense fallback={<div className="empty">Loading conflicts…</div>}>
        <DatabaseConflict
          key={`${path}:${token}`}
          path={path}
          token={token}
          onChanged={onChanged}
          draftScope={draftScope}
        />
      </Suspense>
    );
  }
  if (
    !resolved &&
    data.data &&
    data.data.ours.content.state === "utf8" &&
    data.data.theirs.content.state === "utf8" &&
    ["utf8", "absent"].includes(data.data.base.content.state)
  ) {
    return (
      <Suspense fallback={<div className="empty">Loading conflict…</div>}>
        <TextConflict
          key={`${path}:${token}`}
          path={path}
          token={token}
          expected={data.data.result_etag}
          base={data.data.base.content.content ?? ""}
          current={data.data.ours.content.content ?? ""}
          incoming={data.data.theirs.content.content ?? ""}
          preferences={preferences}
          onChanged={onChanged}
          draftScope={draftScope}
        />
      </Suspense>
    );
  }
  return (
    <>
      <div className="conflict-actions">
        <span>{resolved ? "Resolved" : "Current ↔ Incoming"}</span>
        {!resolved && (
          <>
            <button disabled={busy} onClick={() => setSide("ours")}>
              Use current
            </button>
            <button disabled={busy} onClick={() => setSide("theirs")}>
              Use incoming
            </button>
          </>
        )}
      </div>
      {side && (
        <div className="resolution-confirm" role="alert">
          <span>
            Replace the entire file with the {side === "ours" ? "current" : "incoming"} branch
            version and mark it resolved?
          </span>
          <button disabled={busy} onClick={() => setSide(undefined)}>
            Cancel
          </button>
          <button disabled={busy} onClick={() => void resolve()}>
            {busy ? "Applying…" : "Apply"}
          </button>
        </div>
      )}
      {(error || data.error) && (
        <div className="notice" role="alert">
          {error || data.error}
          {data.error && <button onClick={data.retry}>Retry</button>}
        </div>
      )}
      {!data.data && !data.error && (
        <div className="empty" role="status">
          Loading conflict…
        </div>
      )}
      {data.data &&
        (resolved && !text ? (
          <div className="empty">
            This file has been resolved. Open it in All files to inspect the result.
          </div>
        ) : text ? (
          <Suspense fallback={<div className="empty">Loading diff…</div>}>
            <Code
              path={path}
              pair={{
                before: data.data.ours.content,
                after: resolved ? data.data.result.content : data.data.theirs.content,
              }}
              compare={!resolved}
              editing={false}
              onDraft={() => {}}
              preferences={preferences}
            />
          </Suspense>
        ) : (
          <div className="conflict-records">
            {data.data.conflicts.map((conflict, index) => (
              <ConflictRecord key={index} conflict={conflict} />
            ))}
            {!data.data.conflicts.length && (
              <div className="empty">
                {resolved
                  ? "This file has been resolved."
                  : "No text preview available for this file. Choose a branch version to resolve it."}
              </div>
            )}
            {data.data.next_cursor && (
              <button className="load-more" onClick={() => setAfter(data.data?.next_cursor)}>
                Next conflicts
              </button>
            )}
            {after && (
              <button className="load-more" onClick={() => setAfter(undefined)}>
                First conflicts
              </button>
            )}
          </div>
        ))}
    </>
  );
}
export function FinishMerge({ token, onChanged }: { token: string; onChanged: () => void }) {
  const [message, setMessage] = useState("Merge branches");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  useReviewBusy(busy);
  return (
    <form
      className="finish-merge"
      onSubmit={async (event) => {
        event.preventDefault();
        if (busy || !message.trim()) return;
        setBusy(true);
        setError("");
        try {
          await api("merge-continue", { state_token: token, content: message });
          onChanged();
        } catch (error) {
          setError(error instanceof Error ? error.message : "Could not complete merge");
        } finally {
          setBusy(false);
        }
      }}
    >
      <label>
        All conflicts resolved
        <input
          aria-label="Merge commit message"
          value={message}
          onChange={(event) => setMessage(event.target.value)}
          disabled={busy}
        />
      </label>
      <button disabled={busy || !message.trim()}>{busy ? "Completing…" : "Complete merge"}</button>
      {error && <div role="alert">{error}</div>}
    </form>
  );
}
