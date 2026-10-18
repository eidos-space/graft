import { useEffect, useMemo, useRef, useState } from "react";
import { PagedTable } from "./PagedTable";
import { TableDiff } from "./TableDiff";
import { api, useResource } from "./api";
import { catalogFromData, catalogKey } from "./treeNodes";
import type { DatabaseCatalog, DatabaseDiff, DatabasePage, Mode, RowChange } from "./types";

export function Database({
  path,
  revision,
  compare,
  controlsTarget,
  table,
  mode,
  onCatalog,
}: {
  path: string;
  revision?: string;
  compare: boolean;
  controlsTarget?: HTMLElement | null;
  table?: string;
  mode: Mode;
  onCatalog: (key: string, catalog: DatabaseCatalog) => void;
}) {
  // Publish the catalog as soon as the summary is ready, rather than keeping
  // the explorer waiting for the first page of row differences as well.
  const metadata = useResource<DatabaseCatalog>(
    "database-tables",
    { path, revision },
    false,
    false,
    Boolean(compare && revision && table === undefined),
  );
  const requestedTable = table ?? metadata.data?.default_table ?? metadata.data?.tables[0]?.name;
  const input = { path, revision, table: compare ? requestedTable : table };
  const endpoint = compare ? "sqlite-diff" : "database";
  const resource = useResource<DatabaseDiff | DatabasePage>(
    endpoint,
    input,
    !revision,
    false,
    !compare || !revision || table !== undefined || Boolean(metadata.data || metadata.error),
  );
  const [extra, setExtra] = useState<{
    key: string;
    base: DatabaseDiff | DatabasePage;
    rows: unknown[][];
    changes: RowChange[];
    cursor?: string;
    more: boolean;
  }>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const data = resource.data;
  const catalog = useMemo(() => (data ? catalogFromData(data) : undefined), [data]);
  useEffect(() => {
    if (compare && metadata.data) onCatalog(catalogKey(path, mode, revision), metadata.data);
  }, [compare, metadata.data, path, mode, revision, onCatalog]);
  useEffect(() => {
    if (catalog) onCatalog(catalogKey(path, mode, revision), catalog);
  }, [catalog, path, mode, revision, onCatalog]);
  useEffect(() => {
    // A concurrently dropped table can make its row read fail. Refresh only the
    // names in that case so the explorer can move to a remaining table.
    if (!resource.error) return;
    const controller = new AbortController();
    void api<DatabaseCatalog>(
      "database-tables",
      {
        path,
        revision,
        view: mode === "files" ? "file" : undefined,
      },
      controller.signal,
    )
      .then((next) => {
        if (!controller.signal.aborted) onCatalog(catalogKey(path, mode, revision), next);
      })
      .catch((error: unknown) => {
        if (!controller.signal.aborted)
          onCatalog(catalogKey(path, mode, revision), {
            tables: [],
            error: error instanceof Error ? error.message : "Could not load tables",
          });
      });
    return () => controller.abort();
  }, [resource.error, path, revision, table, mode, onCatalog]);
  const activeExtra = extra?.key === resource.key && extra.base === data ? extra : undefined;
  const pageRequest = useRef<AbortController | null>(null);
  useEffect(() => {
    pageRequest.current?.abort();
    pageRequest.current = null;
    setBusy(false);
    setExtra(undefined);
    setError("");
    return () => pageRequest.current?.abort();
  }, [data, resource.key]);
  const changes = useMemo(
    () =>
      data && "summaries" in data
        ? [...(data.tables?.[0]?.changes ?? []), ...(activeExtra?.changes ?? [])]
        : [],
    [data, activeExtra],
  );
  if (!data)
    return (
      <div className="empty" role="status">
        {resource.error ?? "Loading database…"}
        {resource.error && <button onClick={resource.retry}>Retry</button>}
      </div>
    );
  const diff = "summaries" in data ? data : undefined;
  const database = "columns" in data ? data : undefined;
  const summaries = diff?.summaries ?? [];
  const diffTable = diff?.tables?.[0];
  const selected = table ?? (compare ? (diffTable?.name ?? summaries[0]?.name) : database?.table);
  const schemaChanges = diff?.schema_changes?.filter(
    (item) => selected === undefined || item.name === selected || item.entry_type !== "table",
  );
  const columns = compare ? (diffTable?.columns ?? []) : (database?.columns ?? []);
  const rows = [...(database?.rows ?? []), ...(activeExtra?.rows ?? [])];
  const more = activeExtra
    ? activeExtra.more
    : compare
      ? Boolean(diff?.next_cursor)
      : database?.has_more;
  const summary = summaries.find((item) => item.name === selected);
  const loadMore = async () => {
    if (pageRequest.current) return;
    const controller = new AbortController();
    pageRequest.current = controller;
    setBusy(true);
    setError("");
    try {
      const next = await api<DatabaseDiff | DatabasePage>(
        endpoint,
        {
          ...input,
          table: selected,
          after: compare ? (activeExtra?.cursor ?? diff?.next_cursor) : undefined,
          offset: compare ? undefined : rows.length,
        },
        controller.signal,
      );
      if (controller.signal.aborted) return;
      const nextDiff = "summaries" in next ? next : undefined;
      const nextDatabase = "columns" in next ? next : undefined;
      setExtra({
        key: resource.key,
        base: data,
        rows: [...(activeExtra?.rows ?? []), ...(nextDatabase?.rows ?? [])],
        changes: [...(activeExtra?.changes ?? []), ...(nextDiff?.tables?.[0]?.changes ?? [])],
        cursor: nextDiff?.next_cursor,
        more: compare ? Boolean(nextDiff?.next_cursor) : Boolean(nextDatabase?.has_more),
      });
    } catch (error) {
      if (controller.signal.aborted) return;
      setError(error instanceof Error ? error.message : "Could not load rows");
    } finally {
      if (pageRequest.current === controller) {
        pageRequest.current = null;
        setBusy(false);
      }
    }
  };
  return (
    <div className="database">
      {summary && (
        <div className="table-bar">
          <span className="row-counts">
            <span className="added">+{summary.inserts}</span>
            <span>~{summary.updates}</span>
            <span className="removed">−{summary.deletes}</span>
          </span>
        </div>
      )}
      {(resource.error || error) && (
        <div className="notice" role="alert">
          {resource.error || error}
          {resource.error && <button onClick={resource.retry}>Refresh table</button>}
          {error && <button onClick={() => void loadMore()}>Retry</button>}
        </div>
      )}
      {diff?.message && <div className="notice">{diff.message}</div>}
      {diff?.limitations?.map((item) => (
        <div className="notice" key={`${item.kind}:${item.subject}`}>
          {item.subject ? `${item.subject}: ` : ""}
          {item.kind.replaceAll("_", " ")}
        </div>
      ))}
      {diff?.opaque_changes?.map((item) => (
        <div className="notice" key={item.name}>
          {item.name} · {item.change} · {item.reason.replaceAll("_", " ")}
        </div>
      ))}
      {!!schemaChanges?.length && (
        <details className="schema-changes">
          <summary>Schema changes · {schemaChanges.length}</summary>
          {schemaChanges.map((item) => (
            <div key={`${item.name}:${item.op}`}>
              <span>
                {item.entry_type} · {item.name}
              </span>
              {item.old_sql && <pre className="removed">{item.old_sql}</pre>}
              <pre className={item.op === "delete" ? "removed" : "added"}>{item.sql}</pre>
            </div>
          ))}
        </details>
      )}
      {columns.length ? (
        compare ? (
          <TableDiff
            key={`${resource.key}:${selected}`}
            columns={columns}
            changes={changes}
            controlsTarget={controlsTarget}
            rowidAlias={diffTable?.rowid_alias}
            primaryKeys={diffTable?.primary_key_columns}
            summary={summary}
            more={Boolean(more) && !error}
            busy={busy}
            loadMore={() => void loadMore()}
          />
        ) : (
          <PagedTable key={`${resource.key}:${selected}`} page={database!} input={input} />
        )
      ) : (
        <div className="empty">
          {compare ? "No row changes to preview" : "This database has no tables"}
        </div>
      )}
    </div>
  );
}
