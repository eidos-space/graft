import { useId, useMemo, useState } from "react";
import { createPortal } from "react-dom";
import { DataTable, type TableRow } from "./DataTable";
import { diffIdentityColumns } from "./diffColumns";
import { SegmentedTabs } from "./SegmentedTabs";
import type { RowChange, TableSummary } from "./types";

/** Read-only SQLite row diff. The caller owns ordering, pagination and SDK reads. */
export function TableDiff({
  columns,
  changes,
  rowidAlias,
  primaryKeys,
  more,
  busy,
  loadMore,
  controlsTarget,
  summary,
}: {
  columns: string[];
  changes: RowChange[];
  rowidAlias?: string;
  primaryKeys?: string[];
  more: boolean;
  busy: boolean;
  loadMore: () => void;
  controlsTarget?: HTMLElement | null;
  summary?: TableSummary;
}) {
  const id = useId();
  const [operation, setOperation] = useState<"all" | RowChange["op"]>("all");
  const [fields, setFields] = useState<"all" | "changed">("all");
  const identity = useMemo(
    () => diffIdentityColumns(primaryKeys, rowidAlias, changes),
    [primaryKeys, rowidAlias, changes],
  );
  const rows = useMemo<TableRow[]>(
    () =>
      changes.map((change, row) => ({
        key:
          change.rowid !== undefined
            ? `rowid:${change.rowid}`
            : change.key
              ? JSON.stringify(change.key)
              : String(row),
        operation: change.op,
        cells: [
          { value: change.rowid },
          ...columns.map((column, index) => {
            const alias = column === rowidAlias;
            const before =
              alias && change.rowid !== undefined && change.old_values?.[index] === null
                ? change.rowid
                : change.old_values?.[index];
            const value =
              alias && change.rowid !== undefined && change.values[index] === null
                ? change.rowid
                : change.values[index];
            return {
              value,
              before,
              changed: change.op === "update" && JSON.stringify(before) !== JSON.stringify(value),
              tone:
                change.op === "insert"
                  ? ("added" as const)
                  : change.op === "delete"
                    ? ("removed" as const)
                    : undefined,
            };
          }),
        ],
      })),
    [changes, columns, rowidAlias],
  );
  const visible = useMemo(() => {
    const matching = operation === "all" ? rows : rows.filter((row) => row.operation === operation);
    const indices = columns
      .map((_, index) => index + 1)
      .filter(
        (index) =>
          fields === "all" ||
          identity.primaryKeys.has(columns[index - 1]) ||
          more ||
          matching.some((row) => row.operation !== "update" || row.cells[index].changed),
      );
    return {
      columns: [
        ...(identity.showRowid ? ["rowid"] : []),
        ...indices.map((index) => columns[index - 1]),
      ],
      rows: matching.map((row) => ({
        ...row,
        cells: [
          ...(identity.showRowid ? [row.cells[0]] : []),
          ...indices.map((index) => row.cells[index]),
        ],
      })),
    };
  }, [rows, columns, operation, fields, more, identity]);
  const counts = useMemo(() => {
    if (summary)
      return {
        all: summary.inserts + summary.updates + summary.deletes,
        update: summary.updates,
        insert: summary.inserts,
        delete: summary.deletes,
      };
    const result = { all: rows.length, update: 0, insert: 0, delete: 0 };
    for (const row of rows) if (row.operation) result[row.operation] += 1;
    return result;
  }, [rows, summary]);
  const controls = (
    <div className="table-diff-toolbar">
      <SegmentedTabs
        id={`${id}-rows`}
        label="Row changes"
        panel={`${id}-panel`}
        value={operation}
        onChange={setOperation}
        options={[
          { value: "all", label: "All", count: counts.all },
          { value: "update", label: "Updates", count: counts.update },
          { value: "insert", label: "Added", count: counts.insert },
          { value: "delete", label: "Deleted", count: counts.delete },
        ]}
      />
      <SegmentedTabs
        id={`${id}-fields`}
        label="Fields"
        panel={`${id}-panel`}
        value={fields}
        onChange={setFields}
        options={[
          { value: "all", label: "All fields" },
          { value: "changed", label: "Changed fields" },
        ]}
      />
    </div>
  );
  return (
    <>
      {controlsTarget ? createPortal(controls, controlsTarget) : controls}
      <div
        className="table-diff-panel"
        id={`${id}-panel`}
        role="tabpanel"
        aria-labelledby={`${id}-rows-${operation} ${id}-fields-${fields}`}
      >
        <DataTable
          key={`${operation}:${fields}`}
          columns={visible.columns}
          rows={visible.rows}
          totalRows={visible.rows.length + (more ? 1 : 0)}
          more={more}
          busy={busy}
          loadMore={loadMore}
          emptyMessage="No matching changes"
          diff
        />
      </div>
      {more && (
        <div className="table-progress" role="status">
          {rows.length.toLocaleString()} of {counts.all.toLocaleString()} changes loaded · scroll to
          load more
        </div>
      )}
    </>
  );
}
