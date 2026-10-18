import { useEffect, useState } from "react";
import { api } from "./api";
import { DataTable } from "./DataTable";
import { useReviewDraft } from "./useReviewDraft";
import { clearReviewDraft } from "./reviewDrafts";
import { useReviewBusy } from "./useReviewBusy";
import {
  conflictRowKey,
  databaseConflictRows,
  type ConflictTable,
  type DatabaseRowConflict as Conflict,
  type ConflictSide as Side,
} from "./databaseConflictRows";

interface Page {
  conflicts: Conflict[];
  tables: ConflictTable[];
  next_cursor?: string;
}
interface Field {
  key: string;
  row: string;
  conflict: Conflict;
  column?: string;
  current: unknown;
  incoming: unknown;
  base?: unknown;
  saved?: Side;
}
export function DatabaseConflict({
  path,
  token,
  onChanged,
  draftScope,
}: {
  path: string;
  token: string;
  onChanged: () => void;
  draftScope: string;
}) {
  const [conflicts, setConflicts] = useState<Conflict[]>([]);
  const [loading, setLoading] = useState(true);
  const [cursor, setCursor] = useState<string>();
  const [requestCursor, setRequestCursor] = useState<string>();
  const [retry, setRetry] = useState(0);
  const [busy, setBusy] = useState(false);
  useReviewBusy(busy);
  const [error, setError] = useState("");
  const draftKey = JSON.stringify([draftScope, path, "database"]);
  const [choices, setChoices] = useReviewDraft<Record<string, Side>>(draftKey, {});
  const [rowChoices, setRowChoices] = useReviewDraft<Record<string, Side>>(draftKey + ":rows", {});
  const [tables, setTables] = useState<ConflictTable[]>([]);
  const [selectedTable, setSelectedTable] = useState("");
  const [currentToken, setCurrentToken] = useState(token);
  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    setError("");
    void (async () => {
      try {
        const page = await api<Page>(
          "conflict",
          { path, state_token: token, after: requestCursor },
          controller.signal,
        );
        if (controller.signal.aborted) return;
        setTables((previous) => [
          ...new Map([...previous, ...page.tables].map((table) => [table.name, table])).values(),
        ]);
        setCursor(page.next_cursor);
        setConflicts((previous) =>
          requestCursor ? [...previous, ...page.conflicts] : page.conflicts,
        );
      } catch (cause) {
        if (!controller.signal.aborted) setError(String(cause));
      } finally {
        if (!controller.signal.aborted) setLoading(false);
      }
    })();
    return () => controller.abort();
  }, [path, token, requestCursor, retry]);
  const fields: Field[] = conflicts
    .filter((conflict) => conflict.kind === "row" && conflict.status !== "resolved")
    .flatMap<Field>((conflict) => {
      const row = conflictRowKey(conflict);
      return conflict.cells?.length
        ? conflict.cells.map((cell) => ({
            key: `${row}:${cell.column}`,
            row,
            conflict,
            column: cell.column,
            current: cell.ours,
            incoming: cell.theirs,
            base: cell.base,
            saved: cell.resolution,
          }))
        : [{ key: row, row, conflict, current: conflict.ours_row, incoming: conflict.theirs_row }];
    });
  const unsupported = conflicts.some(
    (conflict) => conflict.status !== "resolved" && conflict.kind !== "row",
  );
  const remaining = fields.filter((field) => !choices[field.key] && !field.saved).length;
  const selected = fields.filter(
    (field) => choices[field.key] && choices[field.key] !== field.saved,
  ).length;
  const table = tables.find((table) => table.name === selectedTable) ?? tables[0];
  const visible = conflicts.filter(
    (conflict) =>
      conflict.kind === "row" && conflict.status !== "resolved" && conflict.table === table?.name,
  );
  const choose = (rowIndex: number, column: number, side: Side) => {
    const conflict = visible[rowIndex];
    if (!conflict || busy) return;
    const wholeRow = column === 0;
    const row = conflictRowKey(conflict);
    const field = table?.columns[column - 1];
    setRowChoices((previous) => {
      const next = { ...previous };
      if (wholeRow) next[row] = side;
      else delete next[row];
      return next;
    });
    setChoices((previous) => ({
      ...previous,
      ...Object.fromEntries(
        fields
          .filter((item) => (wholeRow ? item.row === row : item.key === `${row}:${field}`))
          .map((item) => [item.key, side]),
      ),
    }));
  };
  const save = async () => {
    setBusy(true);
    setError("");
    let stateToken = currentToken;
    const savedRows = new Set<string>();
    try {
      for (const field of fields) {
        if (savedRows.has(field.row)) continue;
        const side = choices[field.key];
        if (!side || (field.saved === side && !rowChoices[field.row])) continue;
        const column = rowChoices[field.row] ? undefined : field.column;
        const outcome = await api<{ merge: { state_token: string } }>(
          column ? "resolve-cell" : "resolve-row",
          {
            path,
            table: field.conflict.table,
            identity: field.conflict.key ?? field.conflict.rowid,
            column,
            action: side,
            state_token: stateToken,
          },
        );
        stateToken = outcome.merge.state_token;
        setCurrentToken(stateToken);
        if (!column) savedRows.add(field.row);
        setChoices((previous) => {
          const next = { ...previous };
          for (const item of fields)
            if (!column ? item.row === field.row : item.key === field.key) delete next[item.key];
          return next;
        });
        setConflicts((previous) =>
          previous.map((conflict) =>
            conflict.table === field.conflict.table &&
            JSON.stringify(conflict.key ?? conflict.rowid) ===
              JSON.stringify(field.conflict.key ?? field.conflict.rowid)
              ? {
                  ...conflict,
                  status: column ? conflict.status : "resolved",
                  cells: conflict.cells?.map((cell) =>
                    cell.column === column ? { ...cell, resolution: side } : cell,
                  ),
                }
              : conflict,
          ),
        );
      }
      if (!cursor && remaining === 0) {
        clearReviewDraft(draftKey);
        clearReviewDraft(draftKey + ":rows");
      }
      onChanged();
    } catch (cause) {
      setError(
        `${String(cause)}. Completed selections have been saved; remaining selections can be retried.`,
      );
    } finally {
      setBusy(false);
    }
  };
  return (
    <>
      <div className="conflict-actions">
        <span role="status">
          {loading
            ? "Loading conflicts…"
            : `${selected} selected · ${remaining} unresolved${cursor ? " in loaded rows" : ""}`}
        </span>
        <button
          disabled={
            loading ||
            busy ||
            !selected ||
            (!cursor && !!remaining) ||
            unsupported ||
            (!!error && !fields.length)
          }
          onClick={() => void save()}
        >
          {busy ? "Saving…" : cursor ? "Save selections" : "Save and mark resolved"}
        </button>
      </div>
      {error && (
        <div className="notice" role="alert">
          {error}
          {!busy && (
            <button
              onClick={() => {
                setRetry((value) => value + 1);
                onChanged();
              }}
            >
              Retry
            </button>
          )}
        </div>
      )}
      {unsupported && (
        <div className="notice">
          This database also has structural conflicts. Resolve those with the CLI before saving row
          selections.
        </div>
      )}
      <div className="conflict-actions" role="tablist" aria-label="Conflicted tables">
        {tables.map((item) => (
          <button
            key={item.name}
            role="tab"
            aria-selected={table?.name === item.name}
            onClick={() => setSelectedTable(item.name)}
          >
            {item.name}
          </button>
        ))}
      </div>
      <DataTable
        key={table?.name}
        columns={["Row", ...(table?.columns ?? [])]}
        rows={table ? databaseConflictRows(visible, table, choices, rowChoices, busy) : []}
        more={false}
        busy={loading}
        loadMore={() => {}}
        onConflictChoice={(column, row, side) => choose(row, column, side)}
        emptyMessage={loading ? "Loading conflicts…" : "No unresolved row conflicts"}
      />
      {cursor && (
        <button
          className="load-more"
          disabled={loading || busy}
          onClick={() => setRequestCursor(cursor)}
        >
          {loading ? "Loading…" : "Load more conflicts"}
        </button>
      )}
    </>
  );
}
