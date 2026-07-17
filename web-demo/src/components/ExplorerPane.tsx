import type { GitStatusEntry } from "@pierre/trees";
import { FileTree, useFileTree } from "@pierre/trees/react";
import { type CSSProperties, useEffect, useMemo, useRef } from "react";
import { useI18n } from "../i18n";
import type { OpfsEntry, RepoStatusPath, RepoTrackedPath } from "../types";

interface ExplorerPaneProps {
  entries: OpfsEntry[];
  onSelect: (path: string) => void;
  selectedPath?: string;
  statusPaths: RepoStatusPath[];
  trackedPaths: RepoTrackedPath[];
}

function isDeletedPath(path: RepoStatusPath) {
  return path.staged_change === "deleted" || path.unstaged_change === "deleted";
}

function treeStatus(paths: RepoStatusPath[]): GitStatusEntry[] {
  return paths.map((entry) => {
    let status: GitStatusEntry["status"] = "modified";
    if (entry.conflicted) status = "modified";
    else if (entry.worktree_status === "untracked") status = "untracked";
    else if (entry.staged_change === "added") status = "added";
    else if (entry.staged_change === "deleted") status = "deleted";
    else if (entry.unstaged_change === "deleted") status = "deleted";
    return { path: entry.path, status };
  });
}

export function ExplorerPane({
  entries,
  onSelect,
  selectedPath,
  statusPaths,
  trackedPaths,
}: ExplorerPaneProps) {
  const { t } = useI18n();
  const containerRef = useRef<HTMLDivElement>(null);
  const onSelectRef = useRef(onSelect);
  onSelectRef.current = onSelect;
  const physicalPaths = useMemo(
    () => new Set(entries.map((entry) => entry.path)),
    [entries],
  );
  const logicalSqlitePaths = useMemo(() => {
    const nextPaths = new Set(
      trackedPaths
        .filter((path) => path.kind === "sqlite_database")
        .map((path) => path.path),
    );
    for (const path of statusPaths) {
      if (path.kind !== "sqlite_database") continue;
      if (isDeletedPath(path)) nextPaths.delete(path.path);
      else nextPaths.add(path.path);
    }
    return nextPaths;
  }, [statusPaths, trackedPaths]);
  const virtualSqlitePaths = useMemo(
    () =>
      new Set(
        [...logicalSqlitePaths].filter((path) => !physicalPaths.has(path)),
      ),
    [logicalSqlitePaths, physicalPaths],
  );
  const paths = useMemo(
    () => [...new Set([...entries.map((entry) => entry.path), ...logicalSqlitePaths])],
    [entries, logicalSqlitePaths],
  );
  const storageStateRef = useRef({
    logicalSqlitePaths,
    materializedTitle: t("tree.materializedTitle"),
    vfsTitle: t("tree.vfsTitle"),
    virtualSqlitePaths,
  });
  storageStateRef.current = {
    logicalSqlitePaths,
    materializedTitle: t("tree.materializedTitle"),
    vfsTitle: t("tree.vfsTitle"),
    virtualSqlitePaths,
  };
  const pathSignature = `${paths.join("\0")}\n${[...virtualSqlitePaths].join("\0")}\n${t("tree.vfsTitle")}\n${t("tree.materializedTitle")}`;
  const { model } = useFileTree({
    density: "compact",
    flattenEmptyDirectories: true,
    initialExpansion: 0,
    onSelectionChange: (paths) => {
      const path = paths.at(-1);
      if (path && !path.endsWith("/")) onSelectRef.current(path);
    },
    paths: [],
    renderRowDecoration: ({ row }) => {
      const state = storageStateRef.current;
      if (row.kind !== "file" || !state.logicalSqlitePaths.has(row.path)) return null;
      return state.virtualSqlitePaths.has(row.path)
        ? { text: "VFS", title: state.vfsTitle }
        : { text: "OPFS", title: state.materializedTitle };
    },
    search: true,
    stickyFolders: true,
    unsafeCSS: `
      [data-item-section="decoration"] > span {
        border: 1px solid currentColor;
        padding: 0 3px;
        font-size: 8px;
        font-weight: 700;
        letter-spacing: 0.08em;
        line-height: 1.45;
      }
    `,
  });

  useEffect(() => {
    model.resetPaths(paths);
    // Only rebuild the model when OPFS paths actually change. Depending on
    // `entries` directly would also reset folder expansion after each refresh.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [model, pathSignature]);

  useEffect(() => {
    model.setGitStatus(treeStatus(statusPaths));
  }, [model, statusPaths]);

  useEffect(() => {
    if (selectedPath) model.getItem(selectedPath)?.select();
  }, [model, selectedPath]);

  useEffect(() => {
    const frame = requestAnimationFrame(() => {
      const host = containerRef.current?.querySelector("file-tree-container");
      const input = host?.shadowRoot?.querySelector<HTMLInputElement>(
        "[data-file-tree-search-input]",
      );
      if (!input) return;
      input.placeholder = t("tree.search");
      input.setAttribute("aria-label", t("tree.search"));
    });
    return () => cancelAnimationFrame(frame);
  }, [t]);

  return (
    <div className="explorer-tree-host" ref={containerRef}>
      <FileTree
        model={model}
        style={
          {
            "--trees-border-color-override": "#b8b8b4",
            "--trees-fg-override": "#111111",
            "--trees-selected-bg-override": "#e8e8e5",
            flex: "1 1 0",
            minHeight: 0,
          } as CSSProperties
        }
      />
    </div>
  );
}
