import type {
  DatabaseCatalog,
  DatabaseDiff,
  DatabasePage,
  DatabaseTable,
  Mode,
  TreeSelection,
} from "./types";

// IDs are internal to the path-based Pierre model. API calls always use the
// associated real file path and original table name, never a virtual path.
export function tableNodeId(path: string, table: string) {
  return `${path}/::table::${encodeURIComponent(table)}`;
}

export function removedTreePaths(previous: Set<string>, next: Set<string>) {
  // Pierre retains inferred parent directories after their last child is
  // removed. Include those parents in the diff so filtering leaves no ghosts.
  const withAncestors = (paths: Set<string>) => {
    const result = new Set(paths);
    for (const path of paths) {
      for (let index = path.indexOf("/"); index >= 0; index = path.indexOf("/", index + 1))
        result.add(path.slice(0, index + 1));
    }
    return result;
  };
  const retained = withAncestors(next);
  // Directory IDs end in a slash. Ignore that trailing empty segment so
  // children are removed before parents during a batched tree update.
  const depth = (path: string) => path.replace(/\/$/, "").split("/").length;
  return [...withAncestors(previous)]
    .filter((path) => !retained.has(path))
    .sort((left, right) => depth(right) - depth(left));
}

export function catalogKey(path: string, mode: Mode, revision?: string) {
  return JSON.stringify([mode === "files" ? "file" : "diff", revision ?? null, path]);
}

/** A failed refresh is not evidence that previously visible tables were dropped. */
export function retainCatalog(
  previous: DatabaseCatalog | undefined,
  next: DatabaseCatalog,
): DatabaseCatalog {
  const tables = next.error && previous ? previous.tables : next.tables;
  const preferred = next.default_table ?? previous?.default_table;
  return {
    ...next,
    tables,
    default_table: tables.some((table) => table.name === preferred) ? preferred : undefined,
  };
}

export function catalogFromData(data: DatabasePage | DatabaseDiff): DatabaseCatalog {
  if ("columns" in data) {
    return { tables: data.tables.map((name) => ({ name })), default_table: data.table };
  }
  const tables = new Map<string, DatabaseTable>();
  for (const summary of data.summaries) tables.set(summary.name, { ...summary });
  for (const change of data.schema_changes ?? []) {
    if (change.entry_type !== "table") continue;
    tables.set(change.name, {
      ...tables.get(change.name),
      name: change.name,
      schema_changed: true,
      change: change.op,
    });
  }
  for (const change of data.opaque_changes ?? []) {
    tables.set(change.name, {
      ...tables.get(change.name),
      name: change.name,
      change: change.change,
      opaque: true,
    });
  }
  return {
    tables: [...tables.values()].sort((left, right) => left.name.localeCompare(right.name)),
    default_table: data.tables?.[0]?.name ?? data.summaries[0]?.name,
  };
}

export type VirtualNode =
  | { kind: "table"; selection: TreeSelection; label: string; summary: DatabaseTable }
  | { kind: "database"; selection: TreeSelection; label: string }
  | { kind: "message"; selection: TreeSelection; label: string; retry: boolean };

export function buildTreeNodes(
  paths: string[],
  databases: Set<string>,
  catalogs: Record<string, DatabaseCatalog>,
  errors: Record<string, string>,
  mode: Mode,
  revision?: string,
) {
  const treePaths: string[] = [];
  const nodes = new Map<string, VirtualNode>();
  for (const path of paths) {
    if (!databases.has(path)) {
      treePaths.push(path);
      continue;
    }
    const parent = `${path}/`;
    treePaths.push(parent);
    nodes.set(parent, {
      kind: "database",
      selection: { path },
      label: path.split("/").at(-1) ?? path,
    });
    const key = catalogKey(path, mode, revision);
    const catalog = catalogs[key];
    if (!catalog?.tables.length) {
      const error = errors[key] ?? catalog?.error;
      const id = `${parent}::status::`;
      treePaths.push(id);
      nodes.set(id, {
        kind: "message",
        selection: { path },
        label: error
          ? "Could not load tables · Retry"
          : catalog
            ? mode === "files"
              ? "No tables"
              : "No changed tables"
            : "Loading tables…",
        retry: Boolean(error),
      });
    }
    for (const table of catalog?.tables ?? []) {
      const id = tableNodeId(path, table.name);
      treePaths.push(id);
      nodes.set(id, {
        kind: "table",
        selection: { path, table: table.name },
        label: table.name || "(unnamed table)",
        summary: table,
      });
    }
  }
  return { paths: treePaths, nodes };
}
