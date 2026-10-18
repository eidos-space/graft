import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, useResource } from "./api";
import type { DiffPreferences } from "./Code";
import { Preview, previewKind } from "./Preview";
import { Tree } from "./RepositoryTree";
import { TreeControls, type TreeActions } from "./TreeControls";
import { readPreference, writePreference } from "./preferences";
import { useWorkspaceLayout } from "./WorkspaceLayout";
import { Branches } from "./Branches";
import { KeyboardHelp } from "./KeyboardHelp";
import { useReviewBusy } from "./useReviewBusy";
import { ViewBoundary } from "./ViewBoundary";
import { ConflictDetail, FinishMerge, type MergePage } from "./Conflicts";
import { catalogKey, retainCatalog } from "./treeNodes";
import type {
  ChangedPath,
  CommitPage,
  DatabaseCatalog,
  FilePair,
  HistoryPage,
  Mode,
  Snapshot,
  TreeSelection,
} from "./types";

const Code = lazy(() => import("./Code").then((module) => ({ default: module.Code })));
const Database = lazy(() => import("./Database").then((module) => ({ default: module.Database })));
const tabs: { mode: Mode; label: string }[] = [
  { mode: "files", label: "All files" },
  { mode: "changes", label: "Changes" },
  { mode: "history", label: "History" },
];

function readDiffPreferences(): DiffPreferences {
  try {
    const stored: unknown = JSON.parse(localStorage.getItem("graft-diff-preferences") ?? "null");
    if (typeof stored === "object" && stored !== null) {
      return {
        wrap: "wrap" in stored && stored.wrap === true,
        layout: "layout" in stored && stored.layout === "split" ? "split" : "unified",
      };
    }
  } catch {
    // Defaults also work when browser storage is unavailable.
  }
  return { wrap: false, layout: "unified" };
}

function firstPath(paths: string[]) {
  return paths.find((path) => /\.(tsx?|jsx?|rs|md|py|json)$/.test(path)) ?? paths[0] ?? "";
}

function FileView({
  path,
  entry,
  database = false,
  mode,
  revision,
  editing,
  setEditing,
  saveRef,
  draftRef,
  onEditable,
  onTextPreview,
  preferences,
  tableControlsTarget,
  table,
  onCatalog,
}: {
  path: string;
  entry?: ChangedPath;
  database?: boolean;
  mode: Mode;
  revision?: string;
  editing: boolean;
  setEditing: (value: boolean) => void;
  saveRef: React.RefObject<(() => Promise<void>) | undefined>;
  draftRef: React.RefObject<string>;
  onEditable: (value: boolean) => void;
  onTextPreview: (value: boolean) => void;
  preferences: DiffPreferences;
  tableControlsTarget: HTMLDivElement | null;
  table?: string;
  onCatalog: (key: string, catalog: DatabaseCatalog) => void;
}) {
  const knownDatabase = database || entry?.kind === "sqlite_database";
  const kind = !revision && !knownDatabase ? previewKind(path) : undefined;
  const resource = useResource<FilePair>(
    "file",
    {
      path,
      revision,
      previous_path: entry?.previous_path,
      view: mode === "files" ? "file" : undefined,
    },
    !revision,
    editing,
    !knownDatabase && !kind,
  );
  const [saved, setSaved] = useState<FilePair>();
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);
  useReviewBusy(saving);
  const original = resource.data;
  const pair = saved ?? original;
  useEffect(() => {
    onEditable(!knownDatabase && !kind && pair?.after.state === "utf8");
    onTextPreview(
      Boolean(
        !knownDatabase &&
        !kind &&
        pair &&
        [pair.before, pair.after].every(
          (content) => content.state === "utf8" || content.state === "absent",
        ) &&
        [pair.before, pair.after].some((content) => content.state === "utf8"),
      ),
    );
    return () => {
      onEditable(false);
      onTextPreview(false);
    };
  }, [knownDatabase, kind, pair?.before.state, pair?.after.state, onEditable, onTextPreview]);
  useEffect(() => {
    setSaved(undefined);
  }, [original]);
  useEffect(() => {
    if (editing) draftRef.current = pair?.after.content ?? "";
    else setError("");
  }, [editing]);
  useEffect(() => {
    const prevent = (event: BeforeUnloadEvent) => {
      if (editing && draftRef.current !== pair?.after.content) event.preventDefault();
    };
    window.addEventListener("beforeunload", prevent);
    return () => window.removeEventListener("beforeunload", prevent);
  }, [editing, pair]);
  const save = useCallback(async () => {
    if (!editing || saving || !pair) return;
    setSaving(true);
    setError("");
    try {
      const content = draftRef.current;
      const response = await api<{ etag: string }>("save", {
        path,
        expected: pair.after.etag,
        content,
      });
      setSaved({
        ...pair,
        after: { state: "utf8", content, etag: response.etag },
      });
      setEditing(false);
    } catch (error) {
      setError(error instanceof Error ? error.message : "Could not save");
    } finally {
      setSaving(false);
    }
  }, [editing, saving, pair, path]);
  useEffect(() => {
    saveRef.current = save;
    return () => {
      saveRef.current = undefined;
    };
  }, [save]);
  const isDatabase =
    knownDatabase || pair?.after.state === "sqlite" || pair?.before.state === "sqlite";
  if (isDatabase && table === undefined)
    return <div className="empty">Expand the database and select a table</div>;
  if (isDatabase)
    return (
      <Database
        key={`${path}:${revision ?? "worktree"}`}
        path={path}
        revision={revision}
        compare={mode !== "files"}
        mode={mode}
        table={table}
        onCatalog={onCatalog}
        controlsTarget={tableControlsTarget}
      />
    );
  if (kind) return <Preview key={path} path={path} kind={kind} />;
  if (!pair)
    return (
      <div className="empty" role="status">
        {resource.error ?? "Loading file…"}
        {resource.error && <button onClick={resource.retry}>Retry</button>}
      </div>
    );
  return (
    <>
      {(error || resource.error) && (
        <div className="notice" role="alert">
          {error || resource.error}
          {resource.error && !editing && <button onClick={resource.retry}>Retry</button>}
        </div>
      )}
      {saving && (
        <div className="saving" role="status">
          Saving…
        </div>
      )}
      <div className="code-surface" inert={saving} aria-busy={saving}>
        <Code
          pair={pair}
          path={path}
          compare={mode !== "files"}
          editing={editing}
          preferences={preferences}
          onDraft={(content) => {
            draftRef.current = content;
          }}
        />
      </div>
    </>
  );
}

export default function App() {
  const [compactFolders, setCompactFolders] = useState(
    () => readPreference("compactFolders") ?? true,
  );
  const filesTree = useRef<TreeActions>(null);
  const changesTree = useRef<TreeActions>(null);
  const historyTree = useRef<TreeActions>(null);
  useEffect(() => {
    writePreference("compactFolders", compactFolders);
  }, [compactFolders]);
  const layout = useWorkspaceLayout();
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const detailPane = useRef<HTMLElement>(null);
  const [epoch, setEpoch] = useState(0);
  const [conflictsOpen, setConflictsOpen] = useState(false);
  const [conflictPath, setConflictPath] = useState("");
  const [mergeAfter, setMergeAfter] = useState<string>();
  const [mode, setMode] = useState<Mode>("changes");
  const [filterOpen, setFilterOpen] = useState(false);
  const [filters, setFilters] = useState<Record<Mode, string>>({
    files: "",
    changes: "",
    history: "",
  });
  const filterInput = useRef<HTMLInputElement>(null);
  const filterReturnFocus = useRef<HTMLElement | null>(null);
  const closeFilter = () => {
    setFilters((previous) => ({ ...previous, [mode]: "" }));
    setFilterOpen(false);
    const target = filterReturnFocus.current;
    if (target?.isConnected) target.focus();
    else document.getElementById(`tab-${mode}`)?.focus();
  };
  useEffect(() => {
    if (filterOpen && !conflictsOpen) filterInput.current?.focus();
  }, [filterOpen, mode, conflictsOpen]);
  const [selection, setSelection] = useState<Record<Mode, TreeSelection>>({
    files: { path: "" },
    changes: { path: "" },
    history: { path: "" },
  });
  const [catalogs, setCatalogs] = useState<Record<string, DatabaseCatalog>>({});
  const onCatalog = useCallback((key: string, catalog: DatabaseCatalog) => {
    setCatalogs((previous) => {
      const next = retainCatalog(previous[key], catalog);
      return JSON.stringify(previous[key]) === JSON.stringify(next)
        ? previous
        : { ...previous, [key]: next };
    });
  }, []);
  const [revision, setRevision] = useState("");
  const [historyExpanded, setHistoryExpanded] = useState(true);
  const [editing, setEditing] = useState(false);
  const [editable, setEditable] = useState(false);
  const [textPreview, setTextPreview] = useState(false);
  const [tableControlsTarget, setTableControlsTarget] = useState<HTMLDivElement | null>(null);
  const [diffPreferences, setDiffPreferences] = useState(readDiffPreferences);
  const [notice, setNotice] = useState("");
  const [reviewBusy, setReviewBusy] = useState(false);
  useEffect(() => {
    const update = () => {
      const busy = Boolean(document.body.dataset.reviewSaving);
      setReviewBusy(busy);
      if (!busy)
        setNotice((previous) =>
          previous === "Saving changes… wait for it to finish before switching views."
            ? ""
            : previous,
        );
    };
    window.addEventListener("graft-review-saving", update);
    return () => window.removeEventListener("graft-review-saving", update);
  }, []);
  const saveRef = useRef<(() => Promise<void>) | undefined>(undefined);
  const draftRef = useRef("");
  useEffect(() => {
    try {
      localStorage.setItem("graft-diff-preferences", JSON.stringify(diffPreferences));
    } catch {
      // Viewing preferences remain available for this session.
    }
  }, [diffPreferences]);
  const snapshot = useResource<Snapshot>("snapshot", {}, true, reviewBusy, true, epoch);
  const merge = useResource<MergePage>(
    "merge",
    { after: mergeAfter },
    true,
    reviewBusy,
    conflictsOpen,
    epoch,
  );
  const repositoryChanged = () => {
    setEpoch((value) => value + 1);
    setCatalogs({});
    setOlder(undefined);
    setRevision("");
    setMergeAfter(undefined);
  };
  useEffect(() => {
    if (!conflictsOpen || !merge.data) return;
    if (merge.data.status.state === "none") {
      setConflictsOpen(false);
      return;
    }
    if (!merge.data.paths.some((item) => item.path === conflictPath))
      setConflictPath(
        merge.data.paths.find((item) => item.state === "unmerged")?.path ??
          merge.data.paths[0]?.path ??
          "",
      );
  }, [merge.data, conflictsOpen, conflictPath]);
  const history = useResource<HistoryPage>(
    "history",
    { filter: filters.history.trim() || undefined },
    mode === "history",
    false,
    mode === "history",
    epoch,
  );
  const [older, setOlder] = useState<HistoryPage & { scope: string }>();
  const activeOlder = older?.scope === history.key ? older : undefined;
  const historyScope = useRef(history.key);
  historyScope.current = history.key;
  const [historyBusy, setHistoryBusy] = useState(false);
  const [morePaths, setMorePaths] = useState<{
    revision: string;
    page: CommitPage;
  }>();
  const [pathsBusy, setPathsBusy] = useState(false);
  const commit = useResource<CommitPage>(
    "commit",
    { revision },
    false,
    false,
    Boolean(revision) && mode === "history",
  );
  const commits = [...(history.data?.commits ?? []), ...(activeOlder?.commits ?? [])];
  const additionalPaths = morePaths?.revision === revision ? morePaths.page : undefined;
  const changes = snapshot.data?.status.paths ?? [];
  const files = snapshot.data?.paths ?? [];
  const commitPaths = [
    ...(revision ? (commit.data?.paths ?? []) : []),
    ...(additionalPaths?.paths ?? []),
  ];
  const activePaths =
    mode === "files"
      ? files
      : mode === "changes"
        ? changes.map((entry) => entry.path)
        : commitPaths.map((entry) => entry.path);
  const selected = selection[mode].path;
  const activeCatalog =
    catalogs[catalogKey(selected, mode, mode === "history" ? revision : undefined)];
  const selectedTable = selection[mode].table;
  const databases = useMemo(
    () => [
      ...new Set([
        ...(snapshot.data?.databases ?? []),
        ...Object.keys(catalogs).map(
          (key) => (JSON.parse(key) as [string, string | null, string])[2],
        ),
      ]),
    ],
    [snapshot.data?.databases, catalogs],
  );
  const treeSelection = (treeMode: Mode): TreeSelection => {
    return selection[treeMode];
  };
  const entries = mode === "history" ? commitPaths : changes;
  const entry = entries.find((entry) => entry.path === selected);

  useEffect(() => {
    if (activePaths.length && !activePaths.includes(selected) && !editing) {
      setSelection((previous) => ({
        ...previous,
        [mode]: { path: firstPath(activePaths) },
      }));
    } else if (!activePaths.length && selected && !editing) {
      setSelection((previous) => ({ ...previous, [mode]: { path: "" } }));
    }
  }, [mode, activePaths.join("\0"), selected, editing]);
  useEffect(() => {
    const table = selection[mode].table;
    if (
      table !== undefined &&
      activeCatalog &&
      !activeCatalog.error &&
      !activeCatalog.tables.some((item) => item.name === table) &&
      !editing
    ) {
      setSelection((previous) => ({ ...previous, [mode]: { path: selected } }));
    }
  }, [mode, selected, selection[mode].table, activeCatalog, editing]);
  useEffect(() => {
    if (!revision && commits[0]) setRevision(commits[0].id);
  }, [history.data]);
  useEffect(() => {
    setMorePaths(undefined);
  }, [revision]);
  useEffect(() => {
    setOlder(undefined);
  }, [history.data?.commits[0]?.id]);

  const navigate = (action: () => void) => {
    if (document.body.dataset.reviewSaving) {
      setNotice("Saving changes… wait for it to finish before switching views.");
      return false;
    }
    if (editing) {
      setNotice("Save or cancel your edit to switch files.");
      return false;
    }
    setNotice("");
    action();
    return true;
  };
  const select = (treeMode: Mode, value: TreeSelection) => {
    return (
      treeMode === mode &&
      navigate(() => setSelection((previous) => ({ ...previous, [treeMode]: value })))
    );
  };
  useEffect(() => {
    const keydown = (event: KeyboardEvent) => {
      if (
        (event.metaKey || event.ctrlKey) &&
        !event.shiftKey &&
        !event.altKey &&
        event.key === "\\"
      ) {
        event.preventDefault();
        if (event.repeat) return;
        if (!sidebarCollapsed && document.activeElement?.closest(".explorer, .pane-resizer"))
          detailPane.current?.focus({ preventScroll: true });
        setSidebarCollapsed((collapsed) => !collapsed);
        return;
      }
      if (
        (event.metaKey || event.ctrlKey) &&
        event.shiftKey &&
        event.key.toLowerCase() === "f" &&
        !conflictsOpen
      ) {
        event.preventDefault();
        setSidebarCollapsed(false);
        if (!filterOpen)
          filterReturnFocus.current =
            document.activeElement instanceof HTMLElement ? document.activeElement : null;
        setFilterOpen(true);
        requestAnimationFrame(() => {
          filterInput.current?.focus();
          filterInput.current?.select();
        });
        return;
      }
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "s" && editing) {
        event.preventDefault();
        void saveRef.current?.();
      }
      if (
        event.key === "Escape" &&
        editing &&
        !document.body.dataset.reviewSaving &&
        !event.defaultPrevented
      ) {
        setEditing(false);
        setNotice("");
      }
    };
    window.addEventListener("keydown", keydown);
    return () => window.removeEventListener("keydown", keydown);
  }, [editing, filterOpen, conflictsOpen, sidebarCollapsed]);
  useEffect(() => {
    document.title = selected
      ? `${selected.split("/").at(-1)} · Graft`
      : `${snapshot.data?.root ?? "Graft"} · Graft`;
  }, [selected, snapshot.data?.root]);

  const loadHistory = async () => {
    const scope = history.key;
    setHistoryBusy(true);
    try {
      const page = await api<HistoryPage>("history", {
        after: activeOlder?.next_cursor ?? history.data?.next_cursor,
        filter: filters.history.trim() || undefined,
      });
      if (historyScope.current !== scope) return;
      setOlder({
        ...page,
        scope,
        commits: [...(activeOlder?.commits ?? []), ...page.commits],
      });
    } catch (error) {
      setNotice(error instanceof Error ? error.message : "Could not load history");
    } finally {
      setHistoryBusy(false);
    }
  };
  const loadPaths = async () => {
    setPathsBusy(true);
    try {
      const page = await api<CommitPage>("commit", {
        revision,
        after: additionalPaths?.next_cursor ?? commit.data?.next_cursor,
      });
      setMorePaths({
        revision,
        page: {
          ...page,
          paths: [...(additionalPaths?.paths ?? []), ...page.paths],
        },
      });
    } catch (error) {
      setNotice(error instanceof Error ? error.message : "Could not load changed files");
    } finally {
      setPathsBusy(false);
    }
  };
  const displayedPath = conflictsOpen ? conflictPath : selected;
  const breadcrumb = displayedPath.split("/");
  if (!snapshot.data && snapshot.error)
    return (
      <main className="session-error">
        <h1>Unable to open this local session</h1>
        <p role="alert">{snapshot.error}</p>
        <button onClick={() => setEpoch((value) => value + 1)}>Retry connection</button>
      </main>
    );
  return (
    <main
      ref={layout.workspaceRef}
      className={`workspace sidebar-${layout.side}${sidebarCollapsed ? " sidebar-collapsed" : ""}`}
      style={layout.style}
    >
      <section ref={detailPane} tabIndex={-1} className="detail" aria-label="File details">
        <header className="file-header">
          <div className="breadcrumb" title={displayedPath || snapshot.data?.root}>
            {displayedPath ? (
              breadcrumb.map((part, index) => (
                <span key={index} className={index === breadcrumb.length - 1 ? "filename" : ""}>
                  {index > 0 && <i>/</i>}
                  {part}
                </span>
              ))
            ) : (
              <span>{snapshot.data?.root ?? "Graft"}</span>
            )}
            {!conflictsOpen && selectedTable !== undefined && (
              <span className="filename">
                <i>/</i>
                {selectedTable || "(unnamed table)"}
              </span>
            )}
          </div>
          {!conflictsOpen && mode === "history" && selected && (
            <span
              className="revision"
              title={commits.find((commit) => commit.id === revision)?.message}
            >
              {revision.slice(0, 7)}
            </span>
          )}
          {((conflictsOpen &&
            merge.data?.paths.find((item) => item.path === conflictPath)?.state === "unmerged" &&
            merge.data?.paths.find((item) => item.path === conflictPath)?.kind === "text_file") ||
            (!conflictsOpen && mode !== "files" && selected && textPreview)) && (
            <div className="diff-controls" role="group" aria-label="Diff display options">
              <button
                className="wrap-toggle"
                aria-label="Wrap"
                aria-pressed={diffPreferences.wrap}
                title="Wrap long lines"
                onClick={() =>
                  setDiffPreferences((previous) => ({
                    ...previous,
                    wrap: !previous.wrap,
                  }))
                }
              >
                <svg
                  width="14"
                  height="14"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="1.5"
                  aria-hidden="true"
                >
                  <path d="M3 5h18M3 10h14a4 4 0 0 1 0 8h-5M3 15h4M3 20h4m8-5-3 3 3 3" />
                </svg>
                <span className="wrap-label">Wrap</span>
              </button>
              <div
                className="diff-layout"
                role="group"
                aria-label="Diff layout"
                hidden={conflictsOpen}
              >
                <button
                  aria-pressed={diffPreferences.layout === "unified"}
                  title="Unified diff · one column"
                  onClick={() =>
                    setDiffPreferences((previous) => ({
                      ...previous,
                      layout: "unified",
                    }))
                  }
                >
                  Unified
                </button>
                <button
                  aria-pressed={diffPreferences.layout === "split"}
                  title="Split diff · side by side"
                  onClick={() =>
                    setDiffPreferences((previous) => ({
                      ...previous,
                      layout: "split",
                    }))
                  }
                >
                  Split
                </button>
              </div>
            </div>
          )}
          <div
            className="table-controls-slot"
            ref={setTableControlsTarget}
            hidden={conflictsOpen}
          />
          {!conflictsOpen &&
            mode === "files" &&
            selected &&
            editable &&
            (editing ? (
              <div className="edit-actions">
                <button
                  disabled={reviewBusy}
                  onClick={() => {
                    setEditing(false);
                    setNotice("");
                  }}
                >
                  Cancel
                </button>
                <button
                  className="save"
                  disabled={reviewBusy}
                  onClick={() => void saveRef.current?.()}
                >
                  {reviewBusy ? "Saving…" : "Save"}
                </button>
              </div>
            ) : (
              <button
                className="edit-button"
                title="Edit text file"
                aria-label="Edit text file"
                onClick={() => setEditing(true)}
              >
                <svg
                  width="15"
                  height="15"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="1.5"
                >
                  <path d="m16 3 5 5-12 12-6 1 1-6L16 3Z" />
                  <path d="m14 5 5 5" />
                </svg>
              </button>
            ))}
        </header>
        {notice && (
          <div className="notice" role="status">
            {notice}
          </div>
        )}
        {snapshot.error && (
          <div className="notice" role="alert">
            {snapshot.error}
          </div>
        )}
        <div className="detail-body">
          <ViewBoundary
            key={`${epoch}:${conflictsOpen}:${conflictPath}:${mode}:${selected}:${revision}:${selectedTable}`}
          >
            {conflictsOpen ? (
              conflictPath && merge.data?.status.state_token ? (
                <ConflictDetail
                  key={conflictPath}
                  path={conflictPath}
                  token={merge.data.status.state_token}
                  resolved={
                    merge.data.paths.find((item) => item.path === conflictPath)?.state ===
                    "resolved"
                  }
                  preferences={diffPreferences}
                  onChanged={repositoryChanged}
                  draftScope={`${merge.data.status.orig_head}:${merge.data.status.merge_head}`}
                  kind={merge.data.paths.find((item) => item.path === conflictPath)?.kind}
                />
              ) : (
                <div className="empty" role="status">
                  {merge.error ?? "Loading conflicts…"}
                </div>
              )
            ) : selected ? (
              <Suspense
                fallback={
                  <div className="empty" role="status">
                    Loading file…
                  </div>
                }
              >
                <FileView
                  key={`${epoch}:${mode}:${selected}:${mode === "history" ? revision : "worktree"}`}
                  path={selected}
                  entry={entry}
                  database={databases.includes(selected)}
                  mode={mode}
                  revision={mode === "history" ? revision : undefined}
                  editing={editing}
                  setEditing={setEditing}
                  saveRef={saveRef}
                  draftRef={draftRef}
                  onEditable={setEditable}
                  onTextPreview={setTextPreview}
                  preferences={diffPreferences}
                  tableControlsTarget={tableControlsTarget}
                  table={selection[mode].table}
                  onCatalog={onCatalog}
                />
              </Suspense>
            ) : (
              <div className="empty" role="status">
                {!snapshot.data
                  ? "Opening worktree…"
                  : mode === "changes"
                    ? "No changes in the worktree"
                    : mode === "history"
                      ? commits.length
                        ? "Select a changed file"
                        : "No commits yet"
                      : "No files in this worktree"}
              </div>
            )}
          </ViewBoundary>
        </div>
      </section>
      {!sidebarCollapsed && layout.separator}
      <aside className="explorer" aria-label="Repository browser" hidden={sidebarCollapsed}>
        <div
          className="tabs"
          role="tablist"
          aria-label="Browse repository"
          onKeyDown={(event) => {
            const buttons = Array.from(
              event.currentTarget.querySelectorAll<HTMLButtonElement>('[role="tab"]'),
            );
            const index = buttons.indexOf(event.target as HTMLButtonElement);
            if (index < 0) return;
            const next =
              event.key === "ArrowRight"
                ? (index + 1) % buttons.length
                : event.key === "ArrowLeft"
                  ? (index + buttons.length - 1) % buttons.length
                  : event.key === "Home"
                    ? 0
                    : event.key === "End"
                      ? buttons.length - 1
                      : undefined;
            if (next === undefined) return;
            event.preventDefault();
            if (editing || document.body.dataset.reviewSaving) return;
            buttons[next].click();
            buttons[next].focus();
          }}
        >
          {tabs.map((tab) => (
            <button
              key={tab.mode}
              role="tab"
              aria-selected={!conflictsOpen && mode === tab.mode}
              tabIndex={!conflictsOpen && mode === tab.mode ? 0 : -1}
              aria-controls={`panel-${tab.mode}`}
              id={`tab-${tab.mode}`}
              onClick={() =>
                navigate(() => {
                  setConflictsOpen(false);
                  setMode(tab.mode);
                })
              }
            >
              {tab.label}
              {tab.mode === "changes" && changes.length > 0 && (
                <span className="count">{changes.length}</span>
              )}
            </button>
          ))}
          {(snapshot.data?.merge?.state === "merging" || conflictsOpen) && (
            <button
              role="tab"
              id="tab-conflicts"
              aria-controls="panel-conflicts"
              aria-selected={conflictsOpen}
              tabIndex={conflictsOpen ? 0 : -1}
              onClick={() => navigate(() => setConflictsOpen(true))}
            >
              Conflicts <span className="count">{snapshot.data?.merge?.unmerged_count ?? 0}</span>
            </button>
          )}
        </div>
        {filterOpen && !conflictsOpen && (
          <div className="explorer-filter">
            <input
              ref={filterInput}
              type="search"
              value={filters[mode]}
              placeholder={
                mode === "history"
                  ? "Filter commits…"
                  : mode === "changes"
                    ? "Filter changes…"
                    : "Filter files…"
              }
              aria-label={
                mode === "history"
                  ? "Filter commits"
                  : mode === "changes"
                    ? "Filter changes"
                    : "Filter files"
              }
              onChange={(event) =>
                setFilters((previous) => ({ ...previous, [mode]: event.target.value }))
              }
              onKeyDown={(event) => {
                if (event.key === "Escape") {
                  event.preventDefault();
                  event.stopPropagation();
                  closeFilter();
                }
              }}
            />
            <button aria-label="Close filter" title="Close filter · Esc" onClick={closeFilter}>
              ×
            </button>
          </div>
        )}
        <div className="explorer-body">
          <div
            className="tree-panel"
            id="panel-files"
            role="tabpanel"
            aria-labelledby="tab-files"
            hidden={conflictsOpen || mode !== "files"}
          >
            <Tree
              compact={compactFolders}
              controlsRef={filesTree}
              paths={files}
              changes={changes}
              selected={treeSelection("files")}
              onSelect={(value) => select("files", value)}
              databases={databases}
              catalogs={catalogs}
              onCatalog={onCatalog}
              mode="files"
              filter={filters.files}
              active={!conflictsOpen && mode === "files"}
            />
            {snapshot.data?.truncated && (
              <div className="tree-note">Showing the first 20,000 files.</div>
            )}
          </div>
          <div
            className="tree-panel"
            id="panel-changes"
            role="tabpanel"
            aria-labelledby="tab-changes"
            hidden={conflictsOpen || mode !== "changes"}
          >
            <Tree
              compact={compactFolders}
              controlsRef={changesTree}
              paths={changes.map((entry) => entry.path)}
              changes={changes}
              selected={treeSelection("changes")}
              onSelect={(value) => select("changes", value)}
              databases={databases}
              catalogs={catalogs}
              onCatalog={onCatalog}
              mode="changes"
              filter={filters.changes}
              active={!conflictsOpen && mode === "changes"}
            />
          </div>
          <div
            className="history-panel"
            id="panel-history"
            role="tabpanel"
            aria-labelledby="tab-history"
            hidden={conflictsOpen || mode !== "history"}
          >
            <ol className="commit-list" aria-label="Commits">
              {commits.map((item) => {
                const active = item.id === revision;
                const expanded = active && historyExpanded;
                return (
                  <li key={item.id} className="commit-item">
                    <button
                      className={`commit ${active ? "selected" : ""}`}
                      aria-expanded={expanded}
                      aria-controls={active ? `commit-files-${item.id}` : undefined}
                      title={item.message}
                      onClick={() =>
                        navigate(() => {
                          if (active) setHistoryExpanded((previous) => !previous);
                          else {
                            setRevision(item.id);
                            setHistoryExpanded(true);
                            setSelection((previous) => ({
                              ...previous,
                              history: { path: "" },
                            }));
                          }
                        })
                      }
                    >
                      <svg
                        className="commit-chevron"
                        width="12"
                        height="12"
                        viewBox="0 0 16 16"
                        fill="none"
                        stroke="currentColor"
                        strokeWidth="1.5"
                        aria-hidden="true"
                      >
                        <path d={expanded ? "m4 6 4 4 4-4" : "m6 4 4 4-4 4"} />
                      </svg>
                      <span className="commit-label">
                        <span className="commit-message">{item.message.split("\n")[0]}</span>
                        <span className="commit-meta">
                          <code>{item.id.slice(0, 7)}</code>
                          <time
                            dateTime={new Date(item.timestamp_ms).toISOString()}
                            title={new Date(item.timestamp_ms).toLocaleString()}
                          >
                            {new Date(item.timestamp_ms).toLocaleDateString(undefined, {
                              month: "short",
                              day: "numeric",
                            })}
                          </time>
                        </span>
                      </span>
                    </button>
                    {active && (
                      <div
                        className="commit-tree"
                        id={`commit-files-${item.id}`}
                        hidden={!expanded}
                      >
                        <Tree
                          compact={compactFolders}
                          controlsRef={historyTree}
                          inline
                          paths={commitPaths.map((entry) => entry.path)}
                          changes={commitPaths}
                          selected={treeSelection("history")}
                          onSelect={(value) => select("history", value)}
                          databases={databases}
                          catalogs={catalogs}
                          onCatalog={onCatalog}
                          mode="history"
                          revision={revision}
                          active={!conflictsOpen && mode === "history" && expanded}
                        />
                        {!commit.data && !commit.error && (
                          <div className="tree-note" role="status">
                            Loading changed files…
                          </div>
                        )}
                        {commit.data && !commitPaths.length && (
                          <div className="tree-note">No changed files</div>
                        )}
                        {(additionalPaths?.has_more ?? commit.data?.has_more) && (
                          <button
                            className="load-more"
                            disabled={pathsBusy}
                            onClick={() => void loadPaths()}
                          >
                            {pathsBusy ? "Loading…" : "More changed files"}
                          </button>
                        )}
                        {commit.error && (
                          <div className="tree-note" role="alert">
                            {commit.error}
                            <button onClick={commit.retry}>Retry</button>
                          </div>
                        )}
                      </div>
                    )}
                  </li>
                );
              })}
            </ol>
            {filters.history.trim() && !commits.length && (
              <div className="tree-note" role="status">
                {history.error ?? (history.data ? "No matching commits" : "Filtering commits…")}
              </div>
            )}
            {(activeOlder?.has_more ?? history.data?.has_more) && (
              <button
                className="load-more"
                disabled={historyBusy}
                onClick={() => void loadHistory()}
              >
                {historyBusy ? "Loading…" : "Older commits"}
              </button>
            )}
            {history.error && (
              <div className="tree-note" role="alert">
                {history.error}
                <button onClick={history.retry}>Retry</button>
              </div>
            )}
          </div>
          {conflictsOpen && (
            <div
              className="conflicts-panel"
              role="tabpanel"
              id="panel-conflicts"
              aria-labelledby="tab-conflicts"
            >
              <div className="merge-caption">
                Current {merge.data?.status.orig_head?.slice(0, 7)} · Incoming{" "}
                {merge.data?.status.merge_head?.slice(0, 7)}
              </div>
              <div className="conflict-list" role="list" aria-label="Merge files">
                {merge.data?.paths.map((item) => (
                  <button
                    key={item.path}
                    aria-pressed={item.path === conflictPath}
                    onClick={() => navigate(() => setConflictPath(item.path))}
                  >
                    <span>{item.path}</span>
                    <span className={item.state === "unmerged" ? "unmerged" : "resolved"}>
                      {item.state === "unmerged" ? "Conflict" : "Resolved"}
                    </span>
                  </button>
                ))}
              </div>
              {merge.data?.next_cursor && (
                <button
                  className="load-more"
                  onClick={() => setMergeAfter(merge.data?.next_cursor)}
                >
                  Next files
                </button>
              )}
              {mergeAfter && (
                <button className="load-more" onClick={() => setMergeAfter(undefined)}>
                  First files
                </button>
              )}
              {merge.error && (
                <div className="tree-note" role="alert">
                  {merge.error}
                  <button onClick={merge.retry}>Retry</button>
                </div>
              )}
              {merge.data?.status.state_token && merge.data.status.unmerged_count === 0 && (
                <FinishMerge token={merge.data.status.state_token} onChanged={repositoryChanged} />
              )}
            </div>
          )}
        </div>
        <div className="explorer-footer">
          <Branches
            epoch={epoch}
            currentBranch={snapshot.data?.status.current_branch}
            onSwitch={repositoryChanged}
            canSwitch={() => navigate(() => {})}
          />
          <KeyboardHelp />
          {!conflictsOpen && (
            <TreeControls
              compact={compactFolders}
              onCompactChange={setCompactFolders}
              onExpand={() => {
                if (mode === "history") setHistoryExpanded(true);
                (mode === "files"
                  ? filesTree
                  : mode === "changes"
                    ? changesTree
                    : historyTree
                ).current?.expandAll();
              }}
              onCollapse={() =>
                (mode === "files"
                  ? filesTree
                  : mode === "changes"
                    ? changesTree
                    : historyTree
                ).current?.collapseAll()
              }
            />
          )}
          <button
            className="sidebar-toggle"
            onClick={layout.toggle}
            aria-label={`Move sidebar to ${layout.side === "left" ? "right" : "left"}`}
            title={`Move sidebar to ${layout.side === "left" ? "right" : "left"}`}
          >
            ⇄
          </button>
        </div>
      </aside>
    </main>
  );
}
