import { useCallback, useEffect, useRef, useState } from "react";
import { DataTable, formatValue, type TableRow } from "./DataTable";
import { CellDiffDialog, type CellComparison } from "./CellDiffDialog";
import { api } from "./api";
import type { ApiInput, DatabasePage } from "./types";
import { TABLE_PAGE_SIZE, TablePageCache, viewportPages } from "./tablePages";

const PAGE_SIZE = TABLE_PAGE_SIZE;
const asRow = (values: unknown[], index: number): TableRow => ({
  key: String(index),
  cells: values.map((value) => ({ value })),
});

/** Random-access reads: dragging the scrollbar never downloads intervening rows. */
export function PagedTable({ page, input }: { page: DatabasePage; input: ApiInput }) {
  const cache = useRef(new TablePageCache<TableRow[]>());
  const identity = useRef(page);
  const [version, setVersion] = useState(0);
  const [range, setRange] = useState({ start: 0, end: 30 });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [retry, setRetry] = useState(0);
  const [detail, setDetail] = useState<CellComparison>();
  const [detailBusy, setDetailBusy] = useState(false);
  const [detailError, setDetailError] = useState("");
  const detailRead = useRef<AbortController | null>(null);
  useEffect(() => () => detailRead.current?.abort(), []);
  const inputKey = JSON.stringify(input);
  useEffect(() => {
    if (identity.current !== page) {
      cache.current.clear();
      identity.current = page;
    }
    const controller = new AbortController();
    const load = async () => {
      setError("");
      setBusy(true);
      try {
        for (const offset of viewportPages(range.start, range.end, page.total_rows)) {
          if (offset === 0 || cache.current.has(offset)) continue;
          const next = await api<DatabasePage>(
            "database",
            { ...input, table: page.table, offset, expected: JSON.stringify(page.version) },
            controller.signal,
          );
          if (controller.signal.aborted) return;
          cache.current.set(
            offset,
            next.rows.map((values, index) => asRow(values, offset + index)),
          );
          setVersion((value) => value + 1);
        }
      } catch (cause) {
        if (!controller.signal.aborted)
          setError(cause instanceof Error ? cause.message : "Could not load rows");
      } finally {
        if (!controller.signal.aborted) setBusy(false);
      }
    };
    void load();
    return () => controller.abort();
  }, [page, inputKey, range, retry]);
  const rowAt = useCallback(
    (index: number) =>
      index < page.rows.length
        ? asRow(page.rows[index], index)
        : identity.current === page
          ? cache.current.get(Math.floor(index / PAGE_SIZE) * PAGE_SIZE)?.[index % PAGE_SIZE]
          : undefined,
    [page, version],
  );
  const visible = useCallback(
    (start: number, end: number) =>
      setRange((previous) =>
        previous.start === start && previous.end === end ? previous : { start, end },
      ),
    [],
  );
  const openCell = async (column: number, row: number) => {
    const record = rowAt(row);
    if (!record) return;
    detailRead.current?.abort();
    const controller = new AbortController();
    detailRead.current = controller;
    setDetail({
      column: page.columns[column],
      row: String(row + 1),
      mode: "value",
      after: formatValue(record.cells[column].value),
    });
    setDetailError("");
    setDetailBusy(true);
    try {
      const result = await api<{ value: unknown }>(
        "database-cell",
        {
          ...input,
          table: page.table,
          column: page.columns[column],
          offset: row,
          expected: JSON.stringify(page.version),
        },
        controller.signal,
      );
      if (!controller.signal.aborted)
        setDetail((previous) => previous && { ...previous, after: formatValue(result.value) });
    } catch (cause) {
      if (!controller.signal.aborted)
        setDetailError(cause instanceof Error ? cause.message : "Could not load value");
    } finally {
      if (!controller.signal.aborted) setDetailBusy(false);
    }
  };
  return (
    <>
      {error && (
        <div className="notice" role="alert">
          {error} <button onClick={() => setRetry((value) => value + 1)}>Retry</button>
        </div>
      )}
      <DataTable
        columns={page.columns}
        rows={page.rows.map(asRow)}
        totalRows={page.total_rows}
        getRow={rowAt}
        onVisibleRange={visible}
        onActivate={(column, row) => void openCell(column, row)}
        more={false}
        busy={busy}
        loadMore={() => {}}
      />
      {page.truncated_cells?.some((columns) => columns.length > 0) && (
        <div className="table-progress">
          Long text is shortened in the grid. Open a cell to read its complete value.
        </div>
      )}
      {detail && (
        <CellDiffDialog
          comparison={detail}
          loading={detailBusy}
          error={detailError}
          onClose={() => {
            detailRead.current?.abort();
            setDetail(undefined);
          }}
        />
      )}
    </>
  );
}
