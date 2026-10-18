import type { GitStatusEntry } from "@pierre/trees";
import { FileTree, useFileTree, useFileTreeSelector } from "@pierre/trees/react";
import {
  useEffect,
  useMemo,
  useRef,
  useState,
  useImperativeHandle,
  type CSSProperties,
  type Ref,
  type RefObject,
} from "react";
import type { TreeActions } from "./TreeControls";
import { treeDirectories, expandableDirectories } from "./treeDirectories";
import { api } from "./api";
import { buildTreeNodes, catalogKey, tableNodeId, removedTreePaths } from "./treeNodes";
import type { ChangedPath, DatabaseCatalog, Mode, TreeSelection } from "./types";

function gitStatus(entry: ChangedPath, path = entry.path): GitStatusEntry {
  const change = entry.change ?? entry.unstaged_change ?? entry.staged_change;
  const status =
    entry.worktree_status === "untracked"
      ? "untracked"
      : change === "added"
        ? "added"
        : change === "deleted"
          ? "deleted"
          : "modified";
  return { path, status };
}

const virtualNodeStyles = `
  [data-graft-node="table"] [data-item-section="content"] > *,
  [data-graft-node="message"] [data-item-section="content"] > * { display: none; }
  [data-graft-node="table"] [data-item-section="content"]::after,
  [data-graft-node="message"] [data-item-section="content"]::after {
    content: attr(data-graft-label); display: block; min-width: 0;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  [data-graft-node="table"] [data-item-section="icon"] svg { display: none; }
  [data-graft-node="table"] [data-item-section="icon"]::after {
    content: ""; width: 12px; height: 12px; border: 1px solid currentColor;
    border-radius: 2px; opacity: .7;
    background: linear-gradient(currentColor, currentColor) 0 3px / 100% 1px no-repeat,
      linear-gradient(currentColor, currentColor) 4px 0 / 1px 100% no-repeat;
  }
  [data-graft-node="message"] { color: var(--fg-muted); }
  [data-graft-node="message"] [data-item-section="icon"] svg { visibility: hidden; }
  [data-graft-node="table"] [data-item-section="decoration"] {
    font-size: 11px; font-variant-numeric: tabular-nums;
  }
`;

type TreeProps = {
  paths: string[];
  changes: ChangedPath[];
  databases: string[];
  catalogs: Record<string, DatabaseCatalog>;
  mode: Mode;
  revision?: string;
  active: boolean;
  selected: TreeSelection;
  onSelect: (selection: TreeSelection) => boolean;
  onCatalog: (key: string, catalog: DatabaseCatalog) => void;
  inline?: boolean;
  filter?: string;
  compact?: boolean;
  controlsRef?: Ref<TreeActions>;
};
interface TreeExpansion {
  expanded?: string[];
  manual: Set<string>;
}

export function Tree(props: TreeProps) {
  const expansion = useRef<TreeExpansion>({ manual: new Set() });
  // Pierre's compact-directory option is constructor-only. Keep the canonical
  // expansion state outside that model so changing it preserves navigation.
  return <TreeContents key={String(props.compact ?? true)} {...props} expansion={expansion} />;
}

function TreeContents({
  paths,
  changes,
  databases,
  catalogs,
  mode,
  revision,
  active,
  selected,
  onSelect,
  onCatalog,
  inline = false,
  filter = "",
  compact = true,
  controlsRef,
  expansion,
}: TreeProps & { expansion: RefObject<TreeExpansion> }) {
  const [errors, setErrors] = useState<Record<string, string>>({});
  const databaseSet = useMemo(
    () =>
      new Set([
        ...databases,
        ...changes.filter((entry) => entry.kind === "sqlite_database").map((entry) => entry.path),
      ]),
    [databases, changes],
  );
  const filteredPaths = useMemo(() => {
    const query = filter.trim().toLocaleLowerCase();
    if (!query) return paths;
    return paths.filter(
      (path) =>
        path.toLocaleLowerCase().includes(query) ||
        catalogs[catalogKey(path, mode, revision)]?.tables.some((table) =>
          table.name.toLocaleLowerCase().includes(query),
        ),
    );
  }, [paths, filter, catalogs, mode, revision]);
  const tree = useMemo(
    () => buildTreeNodes(filteredPaths, databaseSet, catalogs, errors, mode, revision),
    [filteredPaths, databaseSet, catalogs, errors, mode, revision],
  );
  const nodesRef = useRef(tree.nodes);
  nodesRef.current = tree.nodes;
  const callback = useRef(onSelect);
  callback.current = onSelect;
  const syncingSelection = useRef(false);
  const previousPaths = useRef<Set<string>>(new Set());
  const selectedId =
    selected.table !== undefined
      ? tableNodeId(selected.path, selected.table)
      : databaseSet.has(selected.path)
        ? `${selected.path}/`
        : selected.path;
  const selectedRef = useRef(selectedId);
  const syncingTree = useRef(false);
  const expandedState = useRef(new Set<string>());
  const manuallyExpanded = useRef(new Set(expansion.current.manual));
  const directories = useMemo(() => treeDirectories(tree.paths), [tree.paths]);
  const directoriesRef = useRef(directories);
  directoriesRef.current = directories;
  selectedRef.current = selectedId;
  const requests = useRef(new Map<string, AbortController>());
  const loadRef = useRef<(path: string, force?: boolean) => void>(() => {});
  const context = useRef({ catalogs, errors, mode, revision, selected, active, onCatalog });
  context.current = { catalogs, errors, mode, revision, selected, active, onCatalog };

  const { model } = useFileTree({
    paths: [],
    initialExpansion: expansion.current.expanded === undefined ? 2 : 0,
    flattenEmptyDirectories: compact,
    density: "default",
    stickyFolders: !inline,
    search: false,
    unsafeCSS: virtualNodeStyles,
    renderRowDecoration: ({ row }) => {
      const node = nodesRef.current.get(row.path);
      if (node?.kind !== "table") return null;
      const summary = node.summary;
      const parts = [
        ...(summary.inserts ? [{ text: `+${summary.inserts} `, color: "var(--fg-addition)" }] : []),
        ...(summary.updates ? [{ text: `~${summary.updates} `, color: "var(--fg-update)" }] : []),
        ...(summary.deletes ? [{ text: `−${summary.deletes}`, color: "var(--fg-deletion)" }] : []),
      ];
      if (!parts.length && summary.schema_changed)
        parts.push({ text: "Schema", color: "var(--fg-muted)" });
      if (!parts.length && summary.opaque) parts.push({ text: "Opaque", color: "var(--fg-muted)" });
      return parts.length ? { text: parts.map((part) => part.text).join(""), parts } : null;
    },
    onSelectionChange: (selection) => {
      if (syncingSelection.current) return;
      const id = selection.at(-1);
      if (!id || id === selectedRef.current) return;
      if (id.endsWith("/")) {
        model.getItem(id)?.deselect();
        model.getItem(selectedRef.current)?.select();
        return;
      }
      const node = nodesRef.current.get(id);
      if (node?.kind === "message") {
        if (node.retry) loadRef.current(node.selection.path, true);
        model.getItem(id)?.deselect();
        model.getItem(selectedRef.current)?.select();
        return;
      }
      if (!callback.current(node?.selection ?? { path: id })) {
        model.getItem(id)?.deselect();
        model.getItem(selectedRef.current)?.select();
      }
    },
  });
  const contentHeight = useFileTreeSelector(
    model,
    (instance) => instance.getVisibleCount() * instance.getItemHeight(),
  );
  const signature = tree.paths.join("\0");
  const lastExpansionCount = useRef(-1);
  const rememberExpansion = () => {
    lastExpansionCount.current = model.getVisibleCount();
    expansion.current = {
      expanded: directoriesRef.current.filter((path) => {
        const item = model.getItem(path);
        return item && "isExpanded" in item && item.isExpanded();
      }),
      manual: new Set(manuallyExpanded.current),
    };
  };
  useImperativeHandle(
    controlsRef,
    () => ({
      expandAll: () => {
        syncingTree.current = true;
        try {
          const databaseContainers = new Set(
            [...nodesRef.current].filter(([, node]) => node.kind === "database").map(([id]) => id),
          );
          for (const path of expandableDirectories(directoriesRef.current, databaseContainers)) {
            const item = model.getItem(path);
            if (item && "expand" in item) item.expand();
          }
        } finally {
          syncingTree.current = false;
          rememberExpansion();
        }
      },
      collapseAll: () => {
        syncingTree.current = true;
        try {
          for (const path of [...directoriesRef.current].reverse()) {
            const item = model.getItem(path);
            if (item && "collapse" in item) item.collapse();
          }
          manuallyExpanded.current.clear();
          expandedState.current.clear();
        } finally {
          syncingTree.current = false;
          rememberExpansion();
        }
      },
    }),
    [model],
  );
  const expandedDatabases = () =>
    new Set(
      [...nodesRef.current]
        .filter(([id, node]) => {
          const item = model.getItem(id);
          return node.kind === "database" && item && "isExpanded" in item && item.isExpanded();
        })
        .map(([id]) => id),
    );
  useEffect(() => {
    syncingTree.current = true;
    try {
      const previous = previousPaths.current;
      const next = new Set(tree.paths);
      if (!previous.size)
        model.resetPaths(tree.paths, { initialExpandedPaths: expansion.current.expanded });
      else
        model.batch([
          ...removedTreePaths(previous, next).map((path) => ({ type: "remove" as const, path })),
          ...tree.paths
            .filter((path) => !previous.has(path))
            .map((path) => ({ type: "add" as const, path })),
        ]);
      for (const [id, node] of tree.nodes) {
        if (
          node.kind !== "database" ||
          previous.has(id) ||
          expansion.current.expanded?.includes(id)
        )
          continue;
        const item = model.getItem(id);
        if (item && "collapse" in item) item.collapse();
      }
      if (filter.trim()) {
        // Reveal matching files through ordinary folders only. Database
        // containers still require a deliberate disclosure action.
        for (const path of filteredPaths) {
          for (let index = path.indexOf("/"); index >= 0; index = path.indexOf("/", index + 1)) {
            const item = model.getItem(path.slice(0, index + 1));
            if (item && "expand" in item) item.expand();
          }
        }
      }
      previousPaths.current = next;
      for (const id of manuallyExpanded.current)
        if (!next.has(id)) manuallyExpanded.current.delete(id);
    } finally {
      expandedState.current = expandedDatabases();
      syncingTree.current = false;
      rememberExpansion();
    }
  }, [model, signature, filter]);
  // Pierre's built-in search expands matching directories, including database
  // containers. Filter the input paths instead so disclosure stays user-driven.
  const noMatches = Boolean(filter.trim()) && filteredPaths.length === 0;
  useEffect(() => {
    const status = changes.map((entry) =>
      gitStatus(entry, databaseSet.has(entry.path) ? `${entry.path}/` : entry.path),
    );
    for (const [id, node] of tree.nodes) {
      if (node.kind !== "table" || mode === "files") continue;
      status.push({
        path: id,
        status: ["add", "added"].includes(node.summary.change ?? "")
          ? "added"
          : ["delete", "deleted"].includes(node.summary.change ?? "")
            ? "deleted"
            : "modified",
      });
    }
    model.setGitStatus(status);
  }, [model, changes, databaseSet, tree.nodes, mode]);
  useEffect(() => {
    if (!selected.path) return;
    syncingSelection.current = true;
    try {
      for (const id of model.getSelectedPaths()) {
        if (id !== selectedId) model.getItem(id)?.deselect();
      }
      model.getItem(selectedId)?.select();
    } finally {
      syncingSelection.current = false;
    }
  }, [model, selectedId, signature]);

  loadRef.current = (path, force = false) => {
    const current = context.current;
    const key = catalogKey(path, current.mode, current.revision);
    // The active table view publishes metadata already included in its row response.
    if (
      !current.active ||
      requests.current.has(key) ||
      (!force && (current.catalogs[key] || current.errors[key])) ||
      (!force && current.selected.path === path && current.selected.table !== undefined)
    )
      return;
    const controller = new AbortController();
    requests.current.set(key, controller);
    void api<DatabaseCatalog>(
      "database-tables",
      {
        path,
        revision: current.revision,
        view: current.mode === "files" ? "file" : undefined,
      },
      controller.signal,
    )
      .then((catalog) => {
        if (controller.signal.aborted) return;
        current.onCatalog(key, catalog);
        setErrors((previous) => {
          const next = { ...previous };
          delete next[key];
          return next;
        });
      })
      .catch((error: unknown) => {
        if (!controller.signal.aborted)
          setErrors((previous) => ({
            ...previous,
            [key]: error instanceof Error ? error.message : "Could not load tables",
          }));
      })
      .finally(() => {
        if (requests.current.get(key) === controller) requests.current.delete(key);
      });
  };
  useEffect(() => {
    if (!active) return;
    const checkExpanded = (refresh = false) => {
      for (const [id, node] of nodesRef.current) {
        if (node.kind !== "database") continue;
        if (!manuallyExpanded.current.has(id)) continue;
        const item = model.getItem(id);
        if (!item || !("isExpanded" in item) || !item.isExpanded()) continue;
        const key = catalogKey(node.selection.path, mode, revision);
        if (
          refresh &&
          (node.selection.path === context.current.selected.path || context.current.errors[key])
        )
          continue;
        loadRef.current(node.selection.path, refresh);
      }
    };
    const unsubscribe = model.subscribe(() => {
      if (syncingTree.current) return;
      const next = expandedDatabases();
      for (const id of next) if (!expandedState.current.has(id)) manuallyExpanded.current.add(id);
      for (const id of expandedState.current)
        if (!next.has(id)) manuallyExpanded.current.delete(id);
      expandedState.current = next;
      if (model.getVisibleCount() !== lastExpansionCount.current) rememberExpansion();
      checkExpanded();
    });
    checkExpanded();
    const timer =
      mode !== "history"
        ? window.setInterval(() => {
            if (!document.hidden) checkExpanded(true);
          }, 3000)
        : undefined;
    return () => {
      unsubscribe();
      clearInterval(timer);
      for (const controller of requests.current.values()) controller.abort();
      requests.current.clear();
    };
  }, [model, active, mode, revision]);

  useEffect(() => {
    const root = model.getFileTreeContainer()?.shadowRoot;
    if (!root) return;
    // Pierre uses paths as labels. Restore literal SQL names without changing
    // virtual identities or keyboard behavior, including slashes and percent signs.
    const labelRows = () => {
      for (const row of root.querySelectorAll<HTMLElement>("[data-item-path]")) {
        const node = nodesRef.current.get(row.dataset.itemPath ?? "");
        const content = row.querySelector<HTMLElement>('[data-item-section="content"]');
        if (!node) {
          delete row.dataset.graftNode;
          if (content) delete content.dataset.graftLabel;
          row.removeAttribute("title");
          continue;
        }
        row.dataset.graftNode = node.kind;
        if (node.kind === "database") {
          if (content) delete content.dataset.graftLabel;
          row.title = node.selection.path;
          continue;
        }
        // Keep Pierre's React-owned children intact when a virtual row is recycled
        // into an ordinary file row. A CSS label supplies the literal SQL name.
        if (content) content.dataset.graftLabel = node.label;
        if (row.getAttribute("aria-label") !== node.label)
          row.setAttribute("aria-label", node.label);
        row.title =
          node.kind === "message" && node.retry
            ? (context.current.errors[catalogKey(node.selection.path, mode, revision)] ??
              node.label)
            : node.label;
      }
    };
    labelRows();
    const observer = new MutationObserver(labelRows);
    observer.observe(root, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["data-item-path"],
    });
    return () => observer.disconnect();
  }, [model, tree.nodes, mode, revision]);

  return (
    <>
      {noMatches && (
        <div className="tree-note" role="status">
          No matching files
        </div>
      )}
      <FileTree
        hidden={noMatches}
        model={model}
        aria-hidden={!active}
        onKeyDownCapture={
          inline
            ? () =>
                requestAnimationFrame(() => {
                  const item = model.getFileTreeContainer()?.shadowRoot?.activeElement;
                  if (item instanceof HTMLElement && item.getAttribute("role") === "treeitem")
                    item.scrollIntoView({ block: "nearest", inline: "nearest" });
                })
            : undefined
        }
        style={
          {
            "--trees-bg-override": "var(--bg-panel)",
            "--trees-fg-override": "var(--fg)",
            "--trees-fg-muted-override": "var(--fg-muted)",
            "--trees-border-color-override": "var(--border)",
            "--trees-selected-bg-override": "var(--bg-tree-selected)",
            "--trees-selected-fg-override": "var(--fg-tree-selected)",
            "--trees-font-size": "13px",
            "--trees-padding-inline-override": "4px",
            "--trees-item-padding-x-override": "6px",
            height: inline ? contentHeight : "100%",
            width: "100%",
          } as CSSProperties
        }
      />
    </>
  );
}
