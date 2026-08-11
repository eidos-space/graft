import { useEffect, useMemo, useState } from "react";
import { useI18n } from "../i18n";
import type { SqliteDiffView, SqliteRowChange } from "../types";
import { DiffInspectorHeader } from "./DiffInspectorHeader";

function displayValue(value: unknown) {
  if (value === null) return "NULL";
  if (value === undefined) return "—";
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
}

function sameValue(before: unknown, after: unknown) {
  return JSON.stringify(before) === JSON.stringify(after);
}

function ValueDiff({
  after,
  before,
  operation,
}: {
  after: unknown;
  before: unknown;
  operation: SqliteRowChange["op"];
}) {
  const { t } = useI18n();
  if (operation === "insert") {
    return <span className="sqlite-value is-added">{displayValue(after)}</span>;
  }
  if (operation === "delete") {
    return <span className="sqlite-value is-deleted">{displayValue(after)}</span>;
  }
  if (sameValue(before, after)) {
    return <span className="sqlite-value is-unchanged">{displayValue(after)}</span>;
  }
  return (
    <span className="sqlite-value-change">
      <del title={t("sqliteDiff.previous")}>{displayValue(before)}</del>
      <ins title={t("sqliteDiff.new")}>{displayValue(after)}</ins>
    </span>
  );
}

export function SqliteDiffPane({
  diff,
  onClose,
}: {
  diff: SqliteDiffView;
  onClose: () => void;
}) {
  const { t } = useI18n();
  const label =
    diff.label === "HISTORY ROW DIFF"
      ? t("diff.historyRow")
      : diff.label === "ROW DIFF"
        ? t("diff.row")
        : (diff.label ?? t("diff.sqliteRow"));
  const description =
    diff.description === "SQLite row-level changes"
      ? t("diff.sqliteChanges")
      : (diff.description ?? t("diff.comparing"));
  const [tableName, setTableName] = useState(diff.tables[0]?.name ?? "");

  useEffect(() => {
    setTableName(diff.tables[0]?.name ?? "");
  }, [diff]);

  const table = diff.tables.find((item) => item.name === tableName) ?? diff.tables[0];
  const counts = useMemo(
    () =>
      diff.tables.flatMap((item) => item.changes).reduce(
        (current, change) => ({
          ...current,
          [change.op]: current[change.op] + 1,
        }),
        { delete: 0, insert: 0, update: 0 },
      ),
    [diff.tables],
  );
  const total = counts.insert + counts.update + counts.delete;

  return (
    <section
      className="sqlite-diff-surface version-inspector"
      aria-label={t("sqliteDiff.label", { path: diff.path })}
    >
      <DiffInspectorHeader
        mode={diff.label?.startsWith("HISTORY") ? t("version.history") : t("version.changes")}
        onClose={onClose}
        path={diff.path}
      />

      <div className="sqlite-diff-workspace">
        <header className="diff-inspector-toolbar sqlite-diff-toolbar">
          <div>
            <strong>{label}</strong>
            <span>
              {t("sqliteDiff.rows", { count: total })}
              <i aria-hidden="true"> · </i>
              {description}
            </span>
          </div>
          <div className="sqlite-diff-controls">
            <label>
              <span className="sr-only">{t("sqliteDiff.changedTablesAria")}</span>
              <select
                aria-label={t("sqliteDiff.changedTablesAria")}
                onChange={(event) => setTableName(event.target.value)}
                value={table?.name ?? ""}
              >
                {diff.tables.map((item) => (
                  <option key={item.name} value={item.name}>
                    {item.name} ({item.changes.length})
                  </option>
                ))}
              </select>
            </label>
            <div className="sqlite-diff-counts" aria-label={t("sqliteDiff.summary")}>
              <span className="is-insert">+{counts.insert}</span>
              <span className="is-update">±{counts.update}</span>
              <span className="is-delete">−{counts.delete}</span>
            </div>
          </div>
        </header>

        <div className="sqlite-diff-grid-scroll">
          {table ? (
            <table className="sqlite-diff-grid">
              <thead>
                <tr>
                  <th className="operation-column">{t("sqliteDiff.change")}</th>
                  <th className="diff-rowid-column">rowid</th>
                  {table.columns.map((column) => (
                    <th key={column}>{column}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {table.changes.map((change, rowIndex) => (
                  <tr className={`row-diff-${change.op}`} key={`${change.rowid}-${rowIndex}`}>
                    <td className="operation-column">
                      <span className={`operation-badge is-${change.op}`}>
                        {t(
                          change.op === "insert"
                            ? "sqliteDiff.inserted"
                            : change.op === "delete"
                              ? "sqliteDiff.deleted"
                              : "sqliteDiff.updated",
                        )}
                      </span>
                    </td>
                    <td className="diff-rowid-column">{change.rowid}</td>
                    {table.columns.map((column, columnIndex) => (
                      <td key={column}>
                        <ValueDiff
                          after={change.values[columnIndex]}
                          before={change.old_values?.[columnIndex]}
                          operation={change.op}
                        />
                      </td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          ) : (
            <div className="surface-message compact">
              {t("sqliteDiff.noChanges")}
            </div>
          )}
        </div>
      </div>
    </section>
  );
}
