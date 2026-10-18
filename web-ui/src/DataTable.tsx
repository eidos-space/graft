import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import DataEditor, {
  GridCellKind,
  type GridCell,
  type GridColumn,
  type Item,
  type Theme,
} from "@glideapps/glide-data-grid";
import "@glideapps/glide-data-grid/dist/index.css";
import { CellDiffDialog, type CellComparison } from "./CellDiffDialog";
import { useTheme } from "./theme";

export interface TableCell {
  value: unknown;
  before?: unknown;
  changed?: boolean;
  tone?: "added" | "removed";
  conflict?: {
    current: unknown;
    incoming: unknown;
    selected?: "ours" | "theirs";
    disabled?: boolean;
    rowHeader?: boolean;
  };
}
export interface TableRow {
  key: string;
  operation?: "insert" | "update" | "delete";
  cells: TableCell[];
}
export function formatValue(value: unknown): string {
  if (value === null) return "NULL";
  if (value === undefined) return "—";
  if (typeof value === "object") {
    if ("blob_bytes" in value) return `BLOB · ${Number(value.blob_bytes).toLocaleString()} bytes`;
    if ("$blob" in value)
      return `BLOB · ${Math.floor(String(value.$blob).length / 2).toLocaleString()} bytes`;
    return JSON.stringify(value);
  }
  return String(value);
}
export function DataTable({
  columns,
  rows,
  more,
  busy,
  loadMore,
  diff = false,
  emptyMessage = "This table has no rows",
  totalRows,
  getRow,
  onVisibleRange,
  onActivate,
  onConflictChoice,
}: {
  columns: string[];
  rows: TableRow[];
  more: boolean;
  busy: boolean;
  loadMore: () => void;
  diff?: boolean;
  loadedCount?: number;
  emptyMessage?: string;
  totalRows?: number;
  getRow?: (index: number) => TableRow | undefined;
  onVisibleRange?: (start: number, end: number) => void;
  onActivate?: (column: number, row: number) => void;
  onConflictChoice?: (column: number, row: number, side: "ours" | "theirs") => void;
}) {
  const mode = useTheme();
  const host = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });
  const [comparison, setComparison] = useState<CellComparison>();
  const [expandedConflict, setExpandedConflict] = useState<{
    column: number;
    row: number;
    key: string;
  }>();
  const [resized, setResized] = useState<Record<number, number>>({});
  const [range, setRange] = useState({ start: 0, end: 30 });
  const count = totalRows ?? rows.length;
  const rowAt = useCallback(
    (index: number) => (getRow ? getRow(index) : rows[index]),
    [getRow, rows],
  );
  const expandedRow = expandedConflict ? rowAt(expandedConflict.row) : undefined;
  const expandedCell =
    expandedConflict && expandedRow?.key === expandedConflict.key
      ? expandedRow.cells[expandedConflict.column]
      : undefined;
  const expandConflict = ([column, row]: Item) => {
    const record = rowAt(row);
    if (record?.cells[column]?.conflict && !record.cells[column].conflict?.rowHeader)
      setExpandedConflict({ column, row, key: record.key });
  };
  useLayoutEffect(() => {
    const node = host.current;
    if (!node) return;
    const update = () => setSize({ width: node.clientWidth, height: node.clientHeight });
    update();
    const observer = new ResizeObserver(update);
    observer.observe(node);
    return () => observer.disconnect();
  }, []);
  const readPalette = () => {
    const css = getComputedStyle(document.documentElement);
    const color = (name: string) => css.getPropertyValue(name).trim();
    return {
      bg: color("--bg"),
      header: color("--bg-table-header"),
      hover: color("--bg-hover"),
      fg: color("--fg"),
      strong: color("--fg-strong"),
      muted: color("--fg-muted"),
      border: color("--border"),
      focus: color("--focus"),
      selected: color("--bg-tree-selected"),
      add: color("--bg-addition"),
      remove: color("--bg-deletion"),
      green: color("--fg-addition"),
      red: color("--fg-deletion"),
      update: color("--fg-update"),
    };
  };
  const [palette, setPalette] = useState(readPalette);
  useLayoutEffect(() => {
    setPalette(readPalette());
  }, [mode]);
  const theme = useMemo<Partial<Theme>>(
    () => ({
      bgCell: palette.bg,
      bgCellMedium: palette.bg,
      bgHeader: palette.header,
      bgHeaderHovered: palette.hover,
      bgHeaderHasFocus: palette.header,
      textDark: palette.fg,
      textMedium: palette.muted,
      textLight: palette.muted,
      textHeader: palette.strong,
      textHeaderSelected: palette.strong,
      borderColor: palette.border,
      headerBottomBorderColor: palette.border,
      accentColor: palette.focus,
      accentLight: palette.selected,
      accentFg: palette.strong,
      bgBubble: palette.hover,
      bgBubbleSelected: palette.selected,
      textBubble: palette.fg,
      fontFamily: '"SFMono-Regular", Consolas, "Liberation Mono", monospace',
      baseFontStyle: "13px",
      headerFontStyle: "500 12px",
      cellHorizontalPadding: 12,
      cellVerticalPadding: 6,
      editorFontSize: "13px",
    }),
    [palette],
  );
  const sampleKey = JSON.stringify(rows.slice(0, 100));
  const naturalWidths = useMemo(() => {
    const ctx = document.createElement("canvas").getContext("2d");
    if (ctx) ctx.font = '13px "SFMono-Regular", Consolas, monospace';
    const measure = (text: string) => ctx?.measureText(text.slice(0, 80)).width ?? text.length * 7;
    return columns.map((column, index) => {
      if (rows.some((row) => row.cells[index]?.conflict?.rowHeader)) return 120;
      const hasConflict = rows.some((row) => row.cells[index]?.conflict);
      const samples = rows
        .slice(0, 100)
        .map((row) => {
          const cell = row.cells[index];
          return cell?.conflict
            ? Math.max(
                measure(formatValue(cell.conflict.current)),
                measure(formatValue(cell.conflict.incoming)),
              )
            : measure(formatValue(cell?.value));
        })
        .sort((a, b) => a - b);
      return Math.min(
        320,
        Math.max(
          hasConflict ? 220 : 100,
          Math.ceil(Math.max(measure(column), samples[Math.floor(samples.length * 0.9)] ?? 0)) +
            (hasConflict ? 116 : 32),
        ),
      );
    });
  }, [columns, sampleKey]);
  const gridColumns = useMemo<GridColumn[]>(() => {
    const spare =
      Math.max(0, size.width - 36 - naturalWidths.reduce((sum, width) => sum + width, 0)) /
      Math.max(1, columns.length);
    return columns.map((title, index) => ({
      id: String(index),
      title,
      width:
        resized[index] ??
        (onConflictChoice && index === 0 ? 120 : Math.min(360, naturalWidths[index] + spare)),
    }));
  }, [columns, naturalWidths, size.width, resized, onConflictChoice]);
  useEffect(() => {
    if (onVisibleRange) onVisibleRange(range.start, range.end);
    else if (more && !busy && range.end + 30 >= rows.length) loadMore();
  }, [range, onVisibleRange, more, busy, rows.length, loadMore]);
  const content = useCallback(
    ([col, index]: Item): GridCell => {
      const row = rowAt(index);
      if (!row) return { kind: GridCellKind.Loading, allowOverlay: false };
      const cell = row.cells[col];
      const text = cell?.conflict
        ? `Current: ${formatValue(cell.conflict.current)}; Incoming: ${formatValue(cell.conflict.incoming)}; Selected: ${cell.conflict.selected === "ours" ? "Current" : cell.conflict.selected === "theirs" ? "Incoming" : "None"}`
        : formatValue(cell?.value);
      const operation =
        col === 0 && row.operation
          ? `${row.operation === "insert" ? "+" : row.operation === "delete" ? "−" : "~"} `
          : "";
      return {
        kind: GridCellKind.Text,
        data: text,
        displayData: operation + (text || "Empty text"),
        readonly: true,
        allowOverlay: !onActivate && !cell?.changed && !cell?.conflict,
        allowWrapping: true,
        themeOverride:
          cell?.tone === "added"
            ? { bgCell: palette.add, textDark: palette.green }
            : cell?.tone === "removed"
              ? { bgCell: palette.remove, textDark: palette.red }
              : cell?.changed
                ? { textDark: palette.update }
                : undefined,
      };
    },
    [rowAt, palette, onActivate],
  );
  const compareCell = useCallback(
    ([col, index]: Item) => {
      const row = rowAt(index);
      const cell = row?.cells[col];
      if (!row || row.operation !== "update" || !cell?.changed) return;
      setComparison({
        column: columns[col],
        row: row.key,
        before: formatValue(cell.before),
        after: formatValue(cell.value),
      });
    },
    [rowAt, columns],
  );
  return (
    <>
      <div
        ref={host}
        className={`glide-table ${diff ? "table-diff" : ""}`}
        aria-label="Database rows"
      >
        {size.width > 0 && size.height > 0 && count > 0 && (
          <DataEditor
            width={size.width}
            height={size.height}
            columns={gridColumns}
            rows={count}
            getCellContent={content}
            getCellsForSelection={true}
            theme={theme}
            headerHeight={36}
            rowMarkers={onConflictChoice ? "none" : "number"}
            freezeColumns={onConflictChoice ? 1 : 0}
            rowMarkerWidth={36}
            smoothScrollX
            smoothScrollY
            rowHeight={onConflictChoice ? 72 : diff ? 56 : 36}
            onKeyDown={(event) => {
              if (!onConflictChoice || !event.location || !["Enter", " "].includes(event.key))
                return;
              const [column, row] = event.location;
              const conflict = rowAt(row)?.cells[column]?.conflict;
              if (!conflict || conflict.disabled) return;
              event.cancel();
              event.preventDefault();
              event.stopPropagation();
              if (event.key === "Enter" && !conflict.rowHeader) {
                expandConflict(event.location);
                return;
              }
              onConflictChoice(column, row, conflict.selected === "ours" ? "theirs" : "ours");
            }}
            onColumnResize={(_column, width, index) =>
              setResized((previous) => ({ ...previous, [index]: width }))
            }
            onVisibleRegionChanged={(region) =>
              setRange((previous) =>
                previous.start === region.y && previous.end === region.y + region.height
                  ? previous
                  : { start: region.y, end: region.y + region.height },
              )
            }
            onCellActivated={(cell) => {
              if (rowAt(cell[1])?.cells[cell[0]]?.conflict) {
                expandConflict(cell);
                return;
              }
              onActivate ? onActivate(cell[0], cell[1]) : compareCell(cell);
            }}
            onCellClicked={(cell, event) => {
              const conflict = rowAt(cell[1])?.cells[cell[0]]?.conflict;
              if (onConflictChoice && conflict) {
                event.preventDefault();
                if (!conflict.rowHeader && event.localEventX >= 100) {
                  expandConflict(cell);
                  return;
                }
                if (!conflict.disabled)
                  onConflictChoice(
                    cell[0],
                    cell[1],
                    event.localEventY < event.bounds.height / 2 ? "ours" : "theirs",
                  );
                return;
              }
              if (event.localEventX > event.bounds.width - 28) compareCell(cell);
            }}
            drawCell={(args, draw) => {
              const cell = rowAt(args.row)?.cells[args.col];
              if (cell?.conflict) {
                const { ctx, rect } = args;
                const conflict = cell.conflict;
                ctx.save();
                ctx.beginPath();
                ctx.rect(rect.x + 4, rect.y + 2, rect.width - 8, rect.height - 4);
                ctx.clip();
                for (const [index, side] of (["ours", "theirs"] as const).entries()) {
                  const y = rect.y + (index * rect.height) / 2;
                  const selected = conflict.selected === side;
                  if (selected) {
                    ctx.fillStyle = palette.selected;
                    ctx.fillRect(rect.x + 4, y + 3, rect.width - 8, rect.height / 2 - 6);
                  }
                  ctx.strokeStyle = selected ? palette.focus : palette.muted;
                  ctx.beginPath();
                  ctx.arc(rect.x + 16, y + rect.height / 4, 4, 0, Math.PI * 2);
                  ctx.stroke();
                  if (selected) {
                    ctx.fillStyle = palette.focus;
                    ctx.beginPath();
                    ctx.arc(rect.x + 16, y + rect.height / 4, 2, 0, Math.PI * 2);
                    ctx.fill();
                  }
                  ctx.textBaseline = "middle";
                  ctx.font = "11px " + theme.fontFamily;
                  ctx.fillStyle = conflict.disabled
                    ? palette.muted
                    : selected
                      ? palette.strong
                      : palette.muted;
                  ctx.fillText(
                    side === "ours" ? "Current" : "Incoming",
                    rect.x + 28,
                    y + rect.height / 4,
                  );
                  if (!conflict.rowHeader) {
                    ctx.font = theme.baseFontStyle + " " + theme.fontFamily;
                    ctx.fillStyle = conflict.disabled ? palette.muted : palette.fg;
                    ctx.fillText(
                      formatValue(
                        side === "ours" ? conflict.current : conflict.incoming,
                      ).replaceAll("\n", " ↵ "),
                      rect.x + 100,
                      y + rect.height / 4,
                    );
                  }
                }
                ctx.restore();
                if (!conflict.rowHeader) {
                  ctx.fillStyle = palette.header;
                  ctx.fillRect(rect.x + rect.width - 25, rect.y + 1, 24, rect.height - 2);
                  ctx.fillStyle = palette.muted;
                  ctx.font = "13px " + theme.fontFamily;
                  ctx.textBaseline = "middle";
                  ctx.fillText("↗", rect.x + rect.width - 19, rect.y + rect.height / 2);
                }
                return;
              }
              if (!cell?.changed) {
                draw();
                return;
              }
              const { ctx, rect } = args;
              ctx.save();
              ctx.beginPath();
              ctx.rect(rect.x + 8, rect.y, rect.width - 36, rect.height);
              ctx.clip();
              ctx.font = theme.baseFontStyle + " " + theme.fontFamily;
              ctx.textBaseline = "middle";
              ctx.fillStyle = palette.red;
              ctx.fillText(
                "− " + formatValue(cell.before).replaceAll("\n", " "),
                rect.x + 12,
                rect.y + 17,
              );
              ctx.fillStyle = palette.green;
              ctx.fillText(
                "+ " + formatValue(cell.value).replaceAll("\n", " "),
                rect.x + 12,
                rect.y + 39,
              );
              ctx.restore();
              ctx.fillStyle = palette.muted;
              ctx.fillText("↔", rect.x + rect.width - 22, rect.y + rect.height / 2);
            }}
            onDelete={() => false}
            onPaste={false}
          />
        )}
        {count === 0 && <div className="empty">{emptyMessage}</div>}
      </div>
      <div className="table-footer">
        <span role="status">
          {count.toLocaleString()} {diff ? "changes" : "rows"}
          {busy ? " · Loading…" : ""}
        </span>
      </div>
      {comparison && (
        <CellDiffDialog comparison={comparison} onClose={() => setComparison(undefined)} />
      )}
      {expandedConflict && expandedCell?.conflict && onConflictChoice && (
        <CellDiffDialog
          comparison={{
            column: columns[expandedConflict.column],
            row: expandedConflict.key,
            before: formatValue(expandedCell.conflict.current),
            after: formatValue(expandedCell.conflict.incoming),
          }}
          resolution={{
            selected: expandedCell.conflict.selected,
            disabled: expandedCell.conflict.disabled,
            onSelect: (side) =>
              onConflictChoice(expandedConflict.column, expandedConflict.row, side),
          }}
          onClose={() => setExpandedConflict(undefined)}
        />
      )}
    </>
  );
}
