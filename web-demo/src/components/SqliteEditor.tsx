import { useCallback, useEffect, useMemo, useState } from "react";
import { useI18n } from "../i18n";
import type { GraftClient } from "../lib/graftClient";

interface SqliteEditorProps {
  client: GraftClient;
  materialized: boolean;
  onChanged: (path: string) => Promise<void>;
  path: string;
  workspaceVersion: number;
}

interface ColumnInfo {
  name: string;
  pk: boolean;
  type: string;
}

interface DataRow {
  rowid?: string;
  values: string[];
}

function quoteIdentifier(value: string) {
  return `"${value.replaceAll('"', '""')}"`;
}

function sqlValue(value: string, type: string) {
  if (value.toUpperCase() === "NULL") return "NULL";
  const numeric = /(INT|REAL|FLOA|DOUB|NUM|DEC|BOOL)/i.test(type);
  if (numeric && /^[-+]?(?:\d+\.?\d*|\.\d+)$/.test(value.trim())) return value.trim();
  return `'${value.replaceAll("'", "''")}'`;
}

function parsePipeTable(output: string) {
  const lines = output.replace(/\n+$/, "").split("\n");
  if (!lines[0] || lines[0] === "OK") return { headers: [], rows: [] as string[][] };
  return {
    headers: lines[0].split("|"),
    rows: lines.slice(1).filter(Boolean).map((line) => line.split("|")),
  };
}

export function SqliteEditor({
  client,
  materialized,
  onChanged,
  path,
  workspaceVersion,
}: SqliteEditorProps) {
  const { t } = useI18n();
  const [tables, setTables] = useState<string[]>([]);
  const [table, setTable] = useState("");
  const [columns, setColumns] = useState<ColumnInfo[]>([]);
  const [rows, setRows] = useState<DataRow[]>([]);
  const [drafts, setDrafts] = useState<Record<string, string[]>>({});
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [workingRow, setWorkingRow] = useState<string>();
  const [hasRowid, setHasRowid] = useState(true);

  const runSql = useCallback(
    async (sql: string) => {
      const result = await client.run(["--db", path, "sql", sql]);
      if (result.code !== 0) {
        throw new Error(result.stderr.join("\n") || t("sqlite.commandFailed"));
      }
      return result.stdout.join("\n");
    },
    [client, path, t],
  );

  const loadRows = useCallback(
    async (nextTable: string, nextColumns: ColumnInfo[]) => {
      const selected = quoteIdentifier(nextTable);
      let parsed: ReturnType<typeof parsePipeTable>;
      try {
        parsed = parsePipeTable(
          await runSql(`SELECT rowid AS "__rowid__", * FROM ${selected} LIMIT 100;`),
        );
        setHasRowid(true);
        setRows(
          parsed.rows.map((values) => ({ rowid: values[0], values: values.slice(1) })),
        );
      } catch {
        parsed = parsePipeTable(await runSql(`SELECT * FROM ${selected} LIMIT 100;`));
        setHasRowid(false);
        setRows(parsed.rows.map((values) => ({ values })));
      }
      setDrafts({});
      if (parsed.headers.length && parsed.headers.length < nextColumns.length) {
        setError(t("sqlite.renderError"));
      }
    },
    [runSql, t],
  );

  const loadTable = useCallback(
    async (nextTable: string) => {
      if (!nextTable) {
        setColumns([]);
        setRows([]);
        return;
      }
      setLoading(true);
      setError("");
      try {
        const info = parsePipeTable(
          await runSql(`PRAGMA table_info(${quoteIdentifier(nextTable)});`),
        );
        const nextColumns = info.rows.map((values) => ({
          name: values[1] ?? "column",
          pk: values[5] === "1",
          type: values[2] ?? "",
        }));
        setColumns(nextColumns);
        await loadRows(nextTable, nextColumns);
      } catch (nextError) {
        setError(nextError instanceof Error ? nextError.message : String(nextError));
      } finally {
        setLoading(false);
      }
    },
    [loadRows, runSql],
  );

  const loadDatabase = useCallback(async () => {
    setLoading(true);
    setError("");
    try {
      const result = parsePipeTable(
        await runSql(
          "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name;",
        ),
      );
      const nextTables = result.rows.map((row) => row[0]).filter(Boolean);
      setTables(nextTables);
      const nextTable = nextTables.includes(table) ? table : (nextTables[0] ?? "");
      setTable(nextTable);
      await loadTable(nextTable);
    } catch (nextError) {
      setError(nextError instanceof Error ? nextError.message : String(nextError));
    } finally {
      setLoading(false);
    }
  }, [loadTable, runSql, table]);

  useEffect(() => {
    void loadDatabase();
    // Workspace commands can replace the snapshot behind the same path.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path, workspaceVersion]);

  const dirtyRows = useMemo(() => new Set(Object.keys(drafts)), [drafts]);

  async function saveRow(row: DataRow, rowIndex: number) {
    if (!row.rowid) return;
    const key = row.rowid;
    const values = drafts[key] ?? row.values;
    setWorkingRow(key);
    setError("");
    try {
      const assignments = columns
        .map(
          (column, index) =>
            `${quoteIdentifier(column.name)} = ${sqlValue(values[index] ?? "", column.type)}`,
        )
        .join(", ");
      await runSql(
        `UPDATE ${quoteIdentifier(table)} SET ${assignments} WHERE rowid = ${sqlValue(row.rowid, "INTEGER")};`,
      );
      setRows((current) =>
        current.map((currentRow, index) =>
          index === rowIndex ? { ...currentRow, values } : currentRow,
        ),
      );
      setDrafts((current) => {
        const next = { ...current };
        delete next[key];
        return next;
      });
      await onChanged(path);
    } catch (nextError) {
      setError(nextError instanceof Error ? nextError.message : String(nextError));
    } finally {
      setWorkingRow(undefined);
    }
  }

  async function deleteRow(row: DataRow) {
    if (!row.rowid) return;
    setWorkingRow(row.rowid);
    setError("");
    try {
      await runSql(
        `DELETE FROM ${quoteIdentifier(table)} WHERE rowid = ${sqlValue(row.rowid, "INTEGER")};`,
      );
      await loadRows(table, columns);
      await onChanged(path);
    } catch (nextError) {
      setError(nextError instanceof Error ? nextError.message : String(nextError));
    } finally {
      setWorkingRow(undefined);
    }
  }

  async function addRow() {
    setWorkingRow("new");
    setError("");
    try {
      await runSql(`INSERT INTO ${quoteIdentifier(table)} DEFAULT VALUES;`);
      await loadRows(table, columns);
      await onChanged(path);
    } catch (nextError) {
      setError(nextError instanceof Error ? nextError.message : String(nextError));
    } finally {
      setWorkingRow(undefined);
    }
  }

  return (
    <section
      className={`sqlite-surface ${materialized ? "is-materialized" : "is-vfs-only"}`}
      aria-label={t("sqlite.label", { path })}
    >
      <header className="surface-tabbar">
        <div className="surface-file-tab is-sqlite">
          <span className="file-glyph" aria-hidden="true">
            ◉
          </span>
          <strong>{path}</strong>
        </div>
        <div className="surface-actions sqlite-actions">
          <span
            className={`sqlite-storage-badge ${materialized ? "is-materialized" : "is-vfs"}`}
            title={t(materialized ? "sqlite.materializedDescription" : "sqlite.vfsDescription")}
          >
            {t(materialized ? "sqlite.materialized" : "sqlite.vfsOnly")}
          </span>
          <span>{t("sqlite.data")}</span>
          <button disabled={loading} onClick={() => void loadDatabase()} type="button">
            {t("sqlite.refresh")}
          </button>
        </div>
      </header>

      {!materialized && (
        <div className="sqlite-storage-notice" role="status">
          <strong>{t("sqlite.vfsOnly")}</strong>
          <span>{t("sqlite.vfsDescription")}</span>
        </div>
      )}

      <div className="sqlite-workspace">
        <aside className="sqlite-tables">
          <div>{t("sqlite.tables")}</div>
          {tables.map((name) => (
            <button
              aria-current={table === name ? "page" : undefined}
              key={name}
              onClick={() => {
                setTable(name);
                void loadTable(name);
              }}
              type="button"
            >
              <span aria-hidden="true">▦</span>
              {name}
            </button>
          ))}
          {!tables.length && !loading && <p>{t("sqlite.noTables")}</p>}
        </aside>

        <div className="sqlite-data">
          <div className="sqlite-toolbar">
            <div>
              <strong>{table || t("sqlite.noSelection")}</strong>
              <span>{t("sqlite.rowCount", { count: rows.length })}</span>
            </div>
            <button disabled={!table || loading || !hasRowid} onClick={() => void addRow()}>
              {t("sqlite.addRow")}
            </button>
          </div>
          <div className="sqlite-grid-scroll">
            {loading ? (
              <div className="surface-message compact">{t("sqlite.loading")}</div>
            ) : columns.length ? (
              <table className="sqlite-grid">
                <thead>
                  <tr>
                    <th className="rowid-column">rowid</th>
                    {columns.map((column) => (
                      <th key={column.name}>
                        <strong>{column.name}</strong>
                        <small>
                          {column.type || "ANY"}
                          {column.pk ? " · PK" : ""}
                        </small>
                      </th>
                    ))}
                    <th className="row-actions-column" />
                  </tr>
                </thead>
                <tbody>
                  {rows.map((row, rowIndex) => {
                    const key = row.rowid ?? String(rowIndex);
                    const values = drafts[key] ?? row.values;
                    return (
                      <tr key={key}>
                        <td className="rowid-column">{row.rowid ?? "—"}</td>
                        {columns.map((column, columnIndex) => (
                          <td key={column.name}>
                            <input
                              aria-label={t("sqlite.cellLabel", {
                                column: column.name,
                                row: row.rowid ?? rowIndex + 1,
                                table,
                              })}
                              disabled={!hasRowid || workingRow === key}
                              onChange={(event) => {
                                const nextValues = [...values];
                                nextValues[columnIndex] = event.target.value;
                                setDrafts((current) => ({ ...current, [key]: nextValues }));
                              }}
                              value={values[columnIndex] ?? ""}
                            />
                          </td>
                        ))}
                        <td className="row-actions-column">
                          <button
                            disabled={!dirtyRows.has(key) || workingRow === key || !hasRowid}
                            onClick={() => void saveRow(row, rowIndex)}
                            title={t("sqlite.saveRow")}
                            type="button"
                          >
                            {t("sqlite.save")}
                          </button>
                          <button
                            disabled={workingRow === key || !hasRowid}
                            onClick={() => void deleteRow(row)}
                            aria-label={t("sqlite.deleteRow")}
                            title={t("sqlite.deleteRow")}
                            type="button"
                          >
                            ×
                          </button>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            ) : (
              <div className="surface-message compact">{t("sqlite.selectTable")}</div>
            )}
          </div>
        </div>
      </div>

      <footer className="surface-statusbar">
        <span>{hasRowid ? t("sqlite.editable") : t("sqlite.readOnly")}</span>
        {error && <span className="surface-error">{error}</span>}
      </footer>
    </section>
  );
}
