import type { TableRow } from "./DataTable";
export type ConflictSide = "ours" | "theirs";
export interface DatabaseRowConflict {
  kind: string;
  status: string;
  table: string;
  rowid?: number;
  key?: Record<string, unknown>;
  columns?: string[];
  base_row?: unknown[];
  ours_row?: unknown[];
  theirs_row?: unknown[];
  cells?: {
    column: string;
    base: unknown;
    ours: unknown;
    theirs: unknown;
    resolution?: ConflictSide;
  }[];
}
export interface ConflictTable {
  name: string;
  columns: string[];
  rowid_alias?: string;
}
export function conflictRowKey(conflict: DatabaseRowConflict): string {
  return `${conflict.table}:${JSON.stringify(conflict.key ?? conflict.rowid)}`;
}
export function databaseConflictRows(
  conflicts: DatabaseRowConflict[],
  table: ConflictTable,
  choices: Record<string, ConflictSide>,
  rowChoices: Record<string, ConflictSide>,
  busy: boolean,
): TableRow[] {
  return conflicts.map((conflict) => {
    const key = conflictRowKey(conflict);
    const wholeSide = rowChoices[key];
    return {
      key,
      operation: "update",
      cells: [
        {
          value: "Row",
          conflict: {
            current: "Current",
            incoming: "Incoming",
            selected: wholeSide,
            rowHeader: true,
            disabled: busy,
          },
        },
        ...table.columns.map((column, index) => {
          const cell = conflict.cells?.find((cell) => cell.column === column);
          const read = (values: unknown[] | undefined) => {
            if (conflict.key && column in conflict.key) return conflict.key[column];
            if (
              column === table.rowid_alias &&
              conflict.rowid !== undefined &&
              values?.[index] == null
            )
              return conflict.rowid;
            if (!values) return undefined;
            return values[index];
          };
          const current = cell ? cell.ours : read(conflict.ours_row);
          const incoming = cell ? cell.theirs : read(conflict.theirs_row);
          const side = choices[`${key}:${column}`] ?? cell?.resolution;
          if (cell)
            return {
              value: side === "theirs" ? incoming : current,
              conflict: { current, incoming, selected: side, disabled: busy },
            };
          if (wholeSide) return { value: wholeSide === "ours" ? current : incoming };
          if (!conflict.cells?.length && JSON.stringify(current) !== JSON.stringify(incoming))
            return { value: incoming, before: current, changed: true };
          const base = read(conflict.base_row);
          return { value: JSON.stringify(current) === JSON.stringify(base) ? incoming : current };
        }),
      ],
    };
  });
}
