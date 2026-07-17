import { useEffect, useMemo, useState } from "react";
import { useI18n } from "../i18n";
import type { SqliteDiffView, SqliteRowChange } from "../types";

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

export function SqliteDiffPane({ diff }: { diff: SqliteDiffView }) {
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
      className="sqlite-diff-surface"
      aria-label={t("sqliteDiff.label", { path: diff.path })}
    >
      <header className="surface-tabbar">
        <div className="surface-file-tab is-sqlite-diff">
          <span className="file-glyph" aria-hidden="true">
            ▦
          </span>
          <strong>{diff.path}</strong>
        </div>
        <div className="surface-actions sqlite-diff-heading">
          <span>{label}</span>
          <b>{t("sqliteDiff.rows", { count: total })}</b>
        </div>
      </header>

      <div className="sqlite-diff-workspace">
        <aside className="sqlite-diff-tables" aria-label={t("sqliteDiff.changedTablesAria")}>
          <div className="sqlite-diff-section-label">{t("sqliteDiff.changedTables")}</div>
          {diff.tables.map((item) => (
            <button
              aria-current={table?.name === item.name ? "page" : undefined}
              key={item.name}
              onClick={() => setTableName(item.name)}
              type="button"
            >
              <span aria-hidden="true">▦</span>
              <strong>{item.name}</strong>
              <small>{item.changes.length}</small>
            </button>
          ))}
        </aside>

        <div className="sqlite-diff-data">
          <div className="sqlite-diff-summary" aria-label={t("sqliteDiff.summary")}>
            <div className="is-insert">
              <span>+</span>
              <strong>{counts.insert}</strong>
              <small>{t("sqliteDiff.inserted")}</small>
            </div>
            <div className="is-update">
              <span>±</span>
              <strong>{counts.update}</strong>
              <small>{t("sqliteDiff.updated")}</small>
            </div>
            <div className="is-delete">
              <span>−</span>
              <strong>{counts.delete}</strong>
              <small>{t("sqliteDiff.deleted")}</small>
            </div>
          </div>

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
      </div>

      <footer className="surface-statusbar sqlite-diff-status">
        <span>{t("sqliteDiff.database")}</span>
        <span>{t("sqliteDiff.legend")}</span>
        <span>{description}</span>
      </footer>
    </section>
  );
}
