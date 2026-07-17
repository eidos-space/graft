import { type CSSProperties, useCallback, useEffect, useRef, useState } from "react";
import { BinaryDiffPane } from "./components/BinaryDiffPane";
import { CommitFilesPane } from "./components/CommitFilesPane";
import {
  ConflictResolver,
  type ConflictResolutionRequest,
} from "./components/ConflictResolver";
import { DiffPane } from "./components/DiffPane";
import { EditorPane } from "./components/EditorPane";
import { ExplorerPane } from "./components/ExplorerPane";
import {
  QuickstartGuide,
  QUICKSTART_STEPS,
  type GuideAction,
} from "./components/QuickstartGuide";
import { ResetDialog } from "./components/ResetDialog";
import { ResizeHandle } from "./components/ResizeHandle";
import { SqliteDiffPane } from "./components/SqliteDiffPane";
import { SqliteEditor } from "./components/SqliteEditor";
import {
  TerminalPanel,
  type TerminalPanelHandle,
} from "./components/TerminalPanel";
import { VersionPanel, type VersionTab } from "./components/VersionPanel";
import { useI18n } from "./i18n";
import { GraftClient, parseJsonOutput } from "./lib/graftClient";
import { historyChange } from "./lib/history";
import {
  clearOpfs,
  readOpfsText,
  removeOpfsPath,
  scanOpfs,
  writeOpfsAttachments,
  writeOpfsText,
} from "./lib/opfs";
import type {
  BranchInfo,
  BranchList,
  CommitInfo,
  DiffView,
  OpfsEntry,
  RepoLog,
  RepoConflictList,
  RepoStatus,
  RepoTrackedPath,
  RepoTrackedPathList,
  ResetMode,
  SqliteTableDiff,
} from "./types";

type MobilePane = "commit" | "editor" | "sidebar" | "terminal";
type SidebarTab = "files" | "version";
type Surface =
  | { type: "empty" }
  | { path: string; type: "conflict" }
  | { path: string; type: "file" }
  | { path: string; type: "sqlite" }
  | { diff: DiffView; type: "diff" };

const emptyStatus: RepoStatus = {
  counts: { conflicted: 0, staged: 0, unstaged: 0 },
  has_conflicts: false,
  has_staged_changes: false,
  has_unstaged_changes: false,
  paths: [],
  work_in_progress: false,
};

const emptyConflicts: RepoConflictList = {
  conflicts: [],
  paths: [],
};

type JsonTextState = {
  content?: string;
  state: "absent" | "utf8" | "too_large" | "missing_payload" | "invalid_utf8";
};

type JsonDiff = {
  content?: {
    after: JsonTextState;
    before: JsonTextState;
  };
  files?: Array<{
    change: string;
    path: string;
    tables?: SqliteTableDiff[];
  }>;
};

type JsonPathAction = {
  action: string;
  path: string;
};

type JsonMergeOutcome = {
  paths?: JsonPathAction[];
};

type JsonResolveOutcome = {
  remaining_conflicts: number;
};

function textContents(state: JsonTextState | undefined) {
  if (!state || state.state === "absent") return "";
  if (state.state === "utf8") return state.content ?? "";
  return `[${state.state.replaceAll("_", " ")}]`;
}

function commandError(stderr: string[], fallback: string) {
  return stderr.join("\n").trim() || fallback;
}

function clamp(value: number, minimum: number, maximum: number) {
  return Math.min(maximum, Math.max(minimum, value));
}

function savedNumber(key: string, fallback: number) {
  const value = Number(localStorage.getItem(key));
  return Number.isFinite(value) && value > 0 ? value : fallback;
}

function savedProgress() {
  try {
    const value = JSON.parse(localStorage.getItem("graft-guide-progress") ?? "[]");
    if (!Array.isArray(value)) return [];
    const ids = new Set(QUICKSTART_STEPS.map((step) => step.id));
    return value.filter((item): item is string => typeof item === "string" && ids.has(item));
  } catch {
    return [];
  }
}

function isCompactLayout() {
  return window.matchMedia("(max-width: 900px)").matches;
}

function terminalShortcut() {
  return /Mac|iPhone|iPad|iPod/.test(navigator.platform) ? "⌘`" : "Ctrl+`";
}

async function createReadyClient() {
  const nextClient = new GraftClient();
  try {
    await nextClient.ready;
    return nextClient;
  } catch (error) {
    nextClient.terminate();
    throw error;
  }
}

export function App() {
  const { language, setLanguage, t } = useI18n();
  const [client, setClient] = useState(() => new GraftClient());
  const [runtimeReady, setRuntimeReady] = useState(false);
  const [repositoryReady, setRepositoryReady] = useState(false);
  const [status, setStatus] = useState<RepoStatus>(emptyStatus);
  const [conflicts, setConflicts] = useState<RepoConflictList>(emptyConflicts);
  const [trackedPaths, setTrackedPaths] = useState<RepoTrackedPath[]>([]);
  const [history, setHistory] = useState<CommitInfo[]>([]);
  const [branches, setBranches] = useState<BranchInfo[]>([]);
  const [entries, setEntries] = useState<OpfsEntry[]>([]);
  const [surface, setSurface] = useState<Surface>({ type: "empty" });
  const [sidebarTab, setSidebarTab] = useState<SidebarTab>("files");
  const [selectedFilePath, setSelectedFilePath] = useState<string>();
  const [selectedVersionPath, setSelectedVersionPath] = useState<string>();
  const [selectedHistoryId, setSelectedHistoryId] = useState<string>();
  const [versionTab, setVersionTab] = useState<VersionTab>("changes");
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState(() => t("status.startingRuntime"));
  const [mobilePane, setMobilePane] = useState<MobilePane>("editor");
  const [guideOpen, setGuideOpen] = useState(
    () => localStorage.getItem("graft-guide-open") !== "false",
  );
  const [guideProgress, setGuideProgress] = useState<string[]>(savedProgress);
  const [explorerWidth, setExplorerWidth] = useState(() =>
    savedNumber("graft-explorer-width", 292),
  );
  const [commitFilesWidth, setCommitFilesWidth] = useState(() =>
    savedNumber("graft-commit-files-width", 268),
  );
  const [guideWidth, setGuideWidth] = useState(() =>
    savedNumber("graft-guide-width", 344),
  );
  const [terminalHeight, setTerminalHeight] = useState(() =>
    savedNumber("graft-terminal-height", 260),
  );
  const [terminalOpen, setTerminalOpen] = useState(
    () => localStorage.getItem("graft-terminal-open") !== "false",
  );
  const [resetOpen, setResetOpen] = useState(false);
  const [terminalEpoch, setTerminalEpoch] = useState(0);
  const [surfaceEpoch, setSurfaceEpoch] = useState(0);
  const [workspaceVersion, setWorkspaceVersion] = useState(0);
  const refreshToken = useRef(0);
  const refreshRef = useRef<() => Promise<void>>(async () => {});
  const terminalRef = useRef<TerminalPanelHandle>(null);
  const surfaceRef = useRef(surface);
  surfaceRef.current = surface;

  useEffect(() => {
    localStorage.setItem("graft-guide-open", String(guideOpen));
  }, [guideOpen]);

  useEffect(() => {
    localStorage.setItem("graft-guide-progress", JSON.stringify(guideProgress));
  }, [guideProgress]);

  useEffect(() => {
    localStorage.setItem("graft-explorer-width", String(explorerWidth));
    localStorage.setItem("graft-commit-files-width", String(commitFilesWidth));
    localStorage.setItem("graft-guide-width", String(guideWidth));
    localStorage.setItem("graft-terminal-height", String(terminalHeight));
  }, [commitFilesWidth, explorerWidth, guideWidth, terminalHeight]);

  useEffect(() => {
    localStorage.setItem("graft-terminal-open", String(terminalOpen));
  }, [terminalOpen]);

  useEffect(() => {
    if (!runtimeReady) {
      setMessage(t("status.startingRuntime"));
    } else if (!repositoryReady) {
      setMessage(t("status.opfsReady"));
    } else {
      setMessage(
        status.merge_head
          ? status.has_conflicts
            ? t("status.mergeHasConflicts", { branch: status.merge_head.slice(0, 8) })
            : t("status.allConflictsResolved")
          : status.work_in_progress
          ? t(status.paths.length === 1 ? "status.pathChanged" : "status.pathsChanged", {
              count: status.paths.length,
            })
          : t("status.workingTreeClean"),
      );
    }
  }, [language]);

  const openTerminal = useCallback(() => {
    setTerminalOpen(true);
    if (isCompactLayout()) setMobilePane("terminal");
  }, []);

  const closeTerminal = useCallback(() => {
    setTerminalOpen(false);
    if (isCompactLayout()) setMobilePane("editor");
  }, []);

  const toggleTerminal = useCallback(() => {
    if (terminalOpen) closeTerminal();
    else openTerminal();
  }, [closeTerminal, openTerminal, terminalOpen]);

  useEffect(() => {
    const handleShortcut = (event: KeyboardEvent) => {
      if (
        event.code !== "Backquote" ||
        (!event.metaKey && !event.ctrlKey) ||
        event.altKey ||
        event.shiftKey
      ) {
        return;
      }
      event.preventDefault();
      toggleTerminal();
    };
    window.addEventListener("keydown", handleShortcut, true);
    return () => window.removeEventListener("keydown", handleShortcut, true);
  }, [toggleTerminal]);

  const openFile = useCallback((path: string) => {
    setSelectedFilePath(path);
    setSurface({
      path,
      type:
        !path.startsWith(".graft/") && /\.(?:db|sqlite|sqlite3)$/i.test(path)
          ? "sqlite"
          : "file",
    });
    setMobilePane("editor");
  }, []);

  const loadCurrentDiff = useCallback(
    async (path: string, nextStatus = status) => {
      const change = nextStatus.paths.find((item) => item.path === path);
      if (!change) return;
      setSelectedVersionPath(path);
      setSelectedHistoryId(undefined);
      if (change.conflicted) {
        setSidebarTab("version");
        setVersionTab("changes");
        setSurface({ path, type: "conflict" });
        setMobilePane("editor");
        return;
      }
      setBusy(true);
      try {
        let nextDiff: DiffView | null = null;
        if (change.kind === "text_file") {
          if (!nextStatus.current_head) {
            nextDiff = {
              after: await readOpfsText(path),
              before: "",
              description: "Empty tree → worktree",
              kind: change.kind,
              label: "WORKTREE DIFF",
              path,
            };
          } else {
            const result = await client.run([
              "diff",
              "--json",
              "--content",
              nextStatus.current_head,
              "--",
              path,
            ]);
            const payload = parseJsonOutput<JsonDiff>(result);
            nextDiff = {
              after: textContents(payload.content?.after),
              before: textContents(payload.content?.before),
              description: `${nextStatus.current_head.slice(0, 8)} → worktree`,
              kind: change.kind,
              label: "WORKTREE DIFF",
              path,
            };
          }
        } else if (change.kind === "sqlite_database") {
          const args = ["--db", path, "diff", "--json", "--rows"];
          if (change.index_status !== "none" && change.worktree_status === "none") {
            args.push("--staged");
          }
          const payload = parseJsonOutput<JsonDiff>(await client.run(args));
          const file = payload.files?.find((item) => item.path === path);
          nextDiff = {
            description: "SQLite row-level changes",
            kind: change.kind,
            label: "ROW DIFF",
            path,
            tables: file?.tables ?? [],
          };
        } else {
          nextDiff = {
            change: change.staged_change ?? change.unstaged_change ?? "modified",
            description: nextStatus.current_head
              ? `${nextStatus.current_head.slice(0, 8)} → worktree`
              : "Empty tree → worktree",
            kind: change.kind,
            label: "BINARY CHANGE",
            path,
            size: entries.find((entry) => entry.kind === "file" && entry.path === path)?.size,
            storage: change.storage,
          };
        }
        if (nextDiff) {
          setSurface({ diff: nextDiff, type: "diff" });
          setMobilePane("editor");
        }
      } catch (error) {
        setMessage(error instanceof Error ? error.message : String(error));
      } finally {
        setBusy(false);
      }
    },
    [client, entries, status, t],
  );

  const refresh = useCallback(async () => {
    const token = ++refreshToken.current;
    const nextEntries = await scanOpfs();
    if (token !== refreshToken.current) return;
    setEntries(nextEntries);

    const activeSurface = surfaceRef.current;
    if (
      activeSurface.type === "file" &&
      !nextEntries.some((entry) => entry.path === activeSurface.path)
    ) {
      setSurface({ type: "empty" });
      setSelectedFilePath(undefined);
    }

    if (!nextEntries.some((entry) => entry.path === ".graft/")) {
      setRepositoryReady(false);
      setStatus(emptyStatus);
      setConflicts(emptyConflicts);
      setTrackedPaths([]);
      setHistory([]);
      setBranches([]);
      if (
        activeSurface.type === "sqlite" &&
        !nextEntries.some((entry) => entry.path === activeSurface.path)
      ) {
        setSurface({ type: "empty" });
        setSelectedFilePath(undefined);
      }
      setMessage(t("status.opfsReady"));
      return;
    }

    const statusResult = await client.run(["status", "--json"]);
    if (token !== refreshToken.current) return;
    if (statusResult.code !== 0) {
      setRepositoryReady(false);
      setStatus(emptyStatus);
      setConflicts(emptyConflicts);
      setTrackedPaths([]);
      setHistory([]);
      setBranches([]);
      setMessage(commandError(statusResult.stderr, t("status.graftFailed")));
      return;
    }

    const nextStatus = parseJsonOutput<RepoStatus>(statusResult);
    const [logResult, branchResult, trackedResult, conflictResult] = await Promise.all([
      client.run(["log", "--json", "--limit", "24"]),
      client.run(["branch", "--json"]),
      client.run(["ls-files", "--json"]),
      nextStatus.merge_head
        ? client.run(["conflicts", "--json"])
        : Promise.resolve(undefined),
    ]);
    if (token !== refreshToken.current) return;

    // Raw index conflicts intentionally make the normal tracked inventory
    // unavailable in some Graft builds. Status + conflicts remain authoritative
    // during a merge, so keep the workbench usable until the index is resolved.
    const nextTrackedPaths =
      trackedResult.code === 0
        ? parseJsonOutput<RepoTrackedPathList>(trackedResult).paths
        : [];
    if (
      activeSurface.type === "sqlite" &&
      !nextEntries.some((entry) => entry.path === activeSurface.path) &&
      !nextStatus.paths.some(
        (path) => path.kind === "sqlite_database" && path.path === activeSurface.path,
      ) &&
      !nextTrackedPaths.some(
        (path) => path.kind === "sqlite_database" && path.path === activeSurface.path,
      )
    ) {
      setSurface({ type: "empty" });
      setSelectedFilePath(undefined);
    }

    setRepositoryReady(true);
    setWorkspaceVersion((current) => current + 1);
    setStatus(nextStatus);
    setConflicts(
      conflictResult?.code === 0
        ? parseJsonOutput<RepoConflictList>(conflictResult)
        : emptyConflicts,
    );
    setTrackedPaths(nextTrackedPaths);
    setHistory(logResult.code === 0 ? parseJsonOutput<RepoLog>(logResult).commits : []);
    setBranches(
      branchResult.code === 0 ? parseJsonOutput<BranchList>(branchResult).branches : [],
    );
    if (activeSurface.type === "conflict" && !nextStatus.merge_head) {
      setSurface({ type: "empty" });
      setSelectedVersionPath(undefined);
    }
    setMessage(
      nextStatus.merge_head
        ? nextStatus.has_conflicts
          ? t("status.mergeHasConflicts", { branch: nextStatus.merge_head.slice(0, 8) })
          : t("status.allConflictsResolved")
        : nextStatus.work_in_progress
        ? t(
            nextStatus.paths.length === 1 ? "status.pathChanged" : "status.pathsChanged",
            { count: nextStatus.paths.length },
          )
        : t("status.workingTreeClean"),
    );
  }, [client, t]);

  refreshRef.current = refresh;

  useEffect(() => {
    let active = true;
    client.ready
      .then(async () => {
        if (!active) return;
        setRuntimeReady(true);
        await refreshRef.current();
      })
      .catch((error: unknown) => {
        if (!active) return;
        setMessage(error instanceof Error ? error.message : String(error));
      });
    return () => {
      active = false;
      client.terminate();
    };
  }, [client]);

  const runGuiCommand = useCallback(
    async (args: string[], successMessage: string) => {
      setBusy(true);
      try {
        const result = await client.run(args);
        if (result.code !== 0) {
          throw new Error(commandError(result.stderr, t("status.graftFailed")));
        }
        setMessage(successMessage);
        await refresh();
      } catch (error) {
        setMessage(error instanceof Error ? error.message : String(error));
      } finally {
        setBusy(false);
      }
    },
    [client, refresh, t],
  );

  const mergeBranch = useCallback(
    async (branch: string) => {
      setBusy(true);
      try {
        const result = await client.run(["merge", "--json", branch]);
        if (result.code !== 0) {
          throw new Error(commandError(result.stderr, t("status.graftFailed")));
        }
        const payload = parseJsonOutput<JsonMergeOutcome>(result);
        const conflictPath = payload.paths?.find((path) => path.action === "conflicted")?.path;
        setMessage(
          conflictPath
            ? t("status.mergeHasConflicts", { branch })
            : t("status.mergeStarted", { branch }),
        );
        await refresh();
        setSidebarTab("version");
        setVersionTab("changes");
        setSelectedVersionPath(conflictPath);
        setSurface({ path: conflictPath ?? "", type: "conflict" });
        setMobilePane("editor");
      } catch (error) {
        setMessage(error instanceof Error ? error.message : String(error));
      } finally {
        setBusy(false);
      }
    },
    [client, refresh, t],
  );

  const resolveConflict = useCallback(
    async (request: ConflictResolutionRequest) => {
      setBusy(true);
      try {
        const args = ["resolve", "--json", `--${request.resolution}`];
        if (request.row) {
          args.push("--row", request.row.table, String(request.row.rowid));
        }
        args.push(request.path);
        const result = await client.run(args);
        if (result.code !== 0) {
          throw new Error(commandError(result.stderr, t("status.graftFailed")));
        }
        const payload = parseJsonOutput<JsonResolveOutcome>(result);
        setMessage(
          payload.remaining_conflicts === 0
            ? t("status.allConflictsResolved")
            : t("status.conflictResolved", {
                count: payload.remaining_conflicts,
                path: request.path,
              }),
        );
        if (payload.remaining_conflicts === 0) {
          setGuideProgress((current) =>
            current.includes("resolve-conflict")
              ? current
              : [...current, "resolve-conflict"],
          );
        }
        await refresh();
        setSurface({ path: request.path, type: "conflict" });
        setSelectedVersionPath(request.path);
        return true;
      } catch (error) {
        setMessage(error instanceof Error ? error.message : String(error));
        await refresh();
        return false;
      } finally {
        setBusy(false);
      }
    },
    [client, refresh, t],
  );

  const abortMerge = useCallback(async () => {
    setBusy(true);
    try {
      const result = await client.run(["merge", "--json", "--abort"]);
      if (result.code !== 0) {
        throw new Error(commandError(result.stderr, t("status.graftFailed")));
      }
      setSurface({ type: "empty" });
      setSelectedVersionPath(undefined);
      setMessage(t("status.mergeAborted"));
      await refresh();
      return true;
    } catch (error) {
      setMessage(error instanceof Error ? error.message : String(error));
      return false;
    } finally {
      setBusy(false);
    }
  }, [client, refresh, t]);

  const continueMerge = useCallback(
    async (commitMessage: string) => {
      setBusy(true);
      try {
        const result = await client.run([
          "merge",
          "--json",
          "--continue",
          "-m",
          commitMessage,
        ]);
        if (result.code !== 0) {
          throw new Error(commandError(result.stderr, t("status.graftFailed")));
        }
        setSurface({ type: "empty" });
        setSelectedVersionPath(undefined);
        setMessage(t("status.mergeCompleted"));
        setGuideProgress((current) =>
          current.includes("continue-merge") ? current : [...current, "continue-merge"],
        );
        await refresh();
        setSidebarTab("version");
        setVersionTab("history");
        return true;
      } catch (error) {
        setMessage(error instanceof Error ? error.message : String(error));
        return false;
      } finally {
        setBusy(false);
      }
    },
    [client, refresh, t],
  );

  const discardWorktreePaths = useCallback(
    async (paths: string[]) => {
      const changes = paths
        .map((path) => status.paths.find((change) => change.path === path))
        .filter((change): change is RepoStatus["paths"][number] => Boolean(change));
      if (changes.length === 0) return false;

      setBusy(true);
      try {
        for (const change of changes) {
          if (change.unstaged_change === "untracked") {
            await removeOpfsPath(change.path);
            continue;
          }
          const args = ["restore", "--json", change.path];
          const result = await client.run(args);
          if (result.code !== 0) {
            throw new Error(commandError(result.stderr, t("status.graftFailed")));
          }
        }

        const activeSurface = surfaceRef.current;
        if (
          activeSurface.type === "diff" &&
          paths.includes(activeSurface.diff.path)
        ) {
          setSurface({ type: "empty" });
        }
        setSelectedVersionPath(undefined);
        setSurfaceEpoch((current) => current + 1);
        setMessage(
          changes.length === 1
            ? t("status.discardedPath", { path: changes[0].path })
            : t("status.discardedPaths", { count: changes.length }),
        );
        await refresh();
        return true;
      } catch (error) {
        setMessage(error instanceof Error ? error.message : String(error));
        await refresh();
        return false;
      } finally {
        setBusy(false);
      }
    },
    [client, refresh, status.paths, t],
  );

  const resetVersion = useCallback(
    async (target: string, mode: ResetMode) => {
      setBusy(true);
      try {
        const result = await client.run([
          "reset",
          "--json",
          `--${mode}`,
          target,
        ]);
        if (result.code !== 0) {
          throw new Error(commandError(result.stderr, t("status.graftFailed")));
        }

        if (surfaceRef.current.type === "diff") setSurface({ type: "empty" });
        if (mode === "hard") setSurfaceEpoch((current) => current + 1);
        setSelectedHistoryId(undefined);
        setSelectedVersionPath(undefined);
        setMessage(
          t("status.resetVersion", {
            branch: status.current_branch ?? "HEAD",
            mode,
            target: target.slice(0, 8),
          }),
        );
        await refresh();
        return true;
      } catch (error) {
        setMessage(error instanceof Error ? error.message : String(error));
        await refresh();
        return false;
      } finally {
        setBusy(false);
      }
    },
    [client, refresh, status.current_branch, t],
  );

  const saveFile = useCallback(
    async (path: string, contents: string) => {
      await writeOpfsText(path, contents);
      setMessage(t("status.savedFile", { path }));
      await refresh();
    },
    [refresh, t],
  );

  const uploadAttachments = useCallback(
    async (files: File[]) => {
      let paths: string[] = [];
      try {
        paths = await writeOpfsAttachments(files);
      } finally {
        await refresh();
      }
      setMessage(
        paths.length === 1
          ? t("status.uploadedAttachment", { path: paths[0] })
          : t("status.uploadedAttachments", { count: paths.length }),
      );
      return paths;
    },
    [refresh, t],
  );

  const sqliteChanged = useCallback(
    async (path: string) => {
      setMessage(t("status.savedSqlite", { path }));
      await refresh();
    },
    [refresh, t],
  );

  const toggleGuideStep = useCallback((id: string) => {
    setGuideProgress((current) =>
      current.includes(id) ? current.filter((item) => item !== id) : [...current, id],
    );
  }, []);

  const runGuideCommand = useCallback(async (command: string) => {
    openTerminal();
    const terminal = terminalRef.current;
    if (!terminal) {
      setMessage(t("status.terminalStarting"));
      return false;
    }
    try {
      return await terminal.run(command);
    } catch (error) {
      setMessage(error instanceof Error ? error.message : String(error));
      return false;
    }
  }, [openTerminal, t]);

  const openGuideRowDiff = useCallback(async () => {
    const path = status.paths.find(
      (item) => item.kind === "sqlite_database" && item.path === "data.sqlite",
    );
    if (!path) {
      setMessage(t("status.modifyDatabaseFirst"));
      return false;
    }
    setSidebarTab("version");
    setVersionTab("changes");
    await loadCurrentDiff(path.path);
    return true;
  }, [loadCurrentDiff, status.paths, t]);

  const openGuideConflicts = useCallback(async () => {
    if (!status.merge_head) {
      setMessage(t("status.startMergeFirst"));
      return false;
    }
    const path = conflicts.paths.find((item) => item.unresolved > 0)?.path
      ?? conflicts.paths[0]?.path
      ?? status.paths.find((item) => item.conflicted)?.path
      ?? "";
    setSidebarTab("version");
    setVersionTab("changes");
    setSelectedVersionPath(path || undefined);
    setSurface({ path, type: "conflict" });
    setMobilePane("editor");
    return true;
  }, [conflicts.paths, status.merge_head, status.paths, t]);

  const resetAllData = useCallback(async () => {
    setBusy(true);
    setSurface({ type: "empty" });
    setSelectedFilePath(undefined);
    setSelectedVersionPath(undefined);
    setSelectedHistoryId(undefined);
    setVersionTab("changes");
    refreshToken.current += 1;
    let workerStopped = false;
    try {
      // Queue behind any in-flight command, then destroy WasmFS before deleting
      // its backing OPFS entries. A live WasmFS keeps directory/file handles that
      // become invalid when the main thread removes those entries.
      await client.run(["--help"]);
      client.terminate();
      workerStopped = true;
      setRuntimeReady(false);
      await clearOpfs();

      const nextClient = await createReadyClient();
      setClient(nextClient);
      workerStopped = false;
      setRuntimeReady(true);
      setRepositoryReady(false);
      setStatus(emptyStatus);
      setConflicts(emptyConflicts);
      setTrackedPaths([]);
      setHistory([]);
      setBranches([]);
      setEntries([]);
      setGuideProgress([]);
      setGuideOpen(true);
      setSidebarTab("files");
      setMobilePane("sidebar");
      setTerminalEpoch((current) => current + 1);
      setMessage(t("status.allDataDeleted"));
    } catch (error) {
      const resetError = error instanceof Error ? error.message : String(error);
      if (workerStopped) {
        try {
          const recoveryClient = await createReadyClient();
          setClient(recoveryClient);
          setRuntimeReady(true);
        } catch (restartError) {
          const detail =
            restartError instanceof Error ? restartError.message : String(restartError);
          setMessage(`${resetError}\n${detail}`);
          return;
        }
      }
      setMessage(resetError);
    } finally {
      setBusy(false);
      setResetOpen(false);
    }
  }, [client, t]);

  const createDemo = useCallback(async () => {
    setBusy(true);
    setMessage(t("status.creatingRepository"));
    try {
      for (const [args, label] of [
        [["init", "--json"], "initialize repository"],
        [
          [
            "--db",
            "field-notes.sqlite",
            "sql",
            "CREATE TABLE notes(id INTEGER PRIMARY KEY, title TEXT, done INTEGER DEFAULT 0); INSERT INTO notes(title) VALUES ('Run Graft in a Worker'), ('Inspect OPFS files');",
          ],
          "create SQLite data",
        ],
      ] as Array<[string[], string]>) {
        const result = await client.run(args);
        if (result.code !== 0) {
          throw new Error(`${label}: ${commandError(result.stderr, t("status.graftFailed"))}`);
        }
      }

      await writeOpfsText(
        "README.md",
        "# Field notes\n\nThis repository lives entirely in browser OPFS.\n",
      );

      for (const [args, label] of [
        [["add", "--json", "--all"], "stage initial files"],
        [
          ["commit", "--json", "-m", "Create browser field notes"],
          "create initial commit",
        ],
        [
          [
            "--db",
            "field-notes.sqlite",
            "sql",
            "UPDATE notes SET done = 1 WHERE id = 1; INSERT INTO notes(title) VALUES ('Review the version diff');",
          ],
          "edit SQLite data",
        ],
      ] as Array<[string[], string]>) {
        const result = await client.run(args);
        if (result.code !== 0) {
          throw new Error(`${label}: ${commandError(result.stderr, t("status.graftFailed"))}`);
        }
      }

      await writeOpfsText(
        "README.md",
        "# Field notes\n\nThis repository lives entirely in browser OPFS.\n\n- Graft CLI runs as WebAssembly\n- Commands execute off the main thread\n",
      );
      setMessage(t("status.demoCreated"));
      await refresh();
      setSidebarTab("files");
      openFile("README.md");
    } catch (error) {
      setMessage(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }, [client, openFile, refresh, t]);

  const loadHistoryDiff = useCallback(
    async (commit: CommitInfo, path?: string) => {
      const change = historyChange(commit, path);
      if (!change) return;
      setSelectedHistoryId(commit.id);
      setSelectedVersionPath(change.path);
      setMobilePane(path ? "editor" : "commit");
      setBusy(true);
      try {
        if (change.kind === "binary_file") {
          setSurface({
            diff: {
              change: change.change,
              description: `${commit.parent?.slice(0, 8) ?? "empty"} → ${commit.id.slice(0, 8)}`,
              kind: change.kind,
              label: "HISTORY BINARY CHANGE",
              path: change.path,
              storage: change.storage,
            },
            type: "diff",
          });
          return;
        }
        const base =
          change.kind === "sqlite_database"
            ? ["--db", change.path, "diff", "--json"]
            : ["diff", "--json"];
        if (change.kind === "text_file") base.push("--content");
        if (change.kind === "sqlite_database") base.push("--rows");
        const args = commit.parent
          ? [...base, commit.parent, commit.id, "--", change.path]
          : [...base, "--root", commit.id, "--", change.path];
        const payload = parseJsonOutput<JsonDiff>(await client.run(args));
        let nextDiff: DiffView | null = null;
        if (change.kind === "text_file") {
          nextDiff = {
            after: textContents(payload.content?.after),
            before: textContents(payload.content?.before),
            description: `${commit.parent?.slice(0, 8) ?? "empty"} → ${commit.id.slice(0, 8)}`,
            kind: change.kind,
            label: "HISTORY DIFF",
            path: change.path,
          };
        } else if (change.kind === "sqlite_database") {
          const file = payload.files?.find((item) => item.path === change.path);
          nextDiff = {
            description: commit.message,
            kind: change.kind,
            label: "HISTORY ROW DIFF",
            path: change.path,
            tables: file?.tables ?? [],
          };
        }
        if (nextDiff) {
          setSurface({ diff: nextDiff, type: "diff" });
        }
      } catch (error) {
        setMessage(error instanceof Error ? error.message : String(error));
      } finally {
        setBusy(false);
      }
    },
    [client, t],
  );

  const openGuideHistory = useCallback(async () => {
    if (history.length === 0) {
      setMessage(t("status.commitFirst"));
      return false;
    }
    setSidebarTab("version");
    setVersionTab("history");
    await loadHistoryDiff(history[0]);
    return true;
  }, [history, loadHistoryDiff, t]);

  const runGuideAction = useCallback(
    async (action: GuideAction) => {
      if (action === "open-row-diff") return openGuideRowDiff();
      if (action === "open-conflicts") return openGuideConflicts();
      return openGuideHistory();
    },
    [openGuideConflicts, openGuideHistory, openGuideRowDiff],
  );

  const totalBytes = entries.reduce((sum, entry) => sum + (entry.size ?? 0), 0);
  const opfsFileCount = entries.filter((entry) => entry.kind === "file").length;
  const selectedEntry =
    surface.type === "file" || surface.type === "sqlite"
      ? entries.find((entry) => entry.path === surface.path)
      : undefined;
  const selectedHistoryCommit = history.find(
    (commit) => commit.id === selectedHistoryId,
  );
  const historyReviewOpen =
    repositoryReady && sidebarTab === "version" && versionTab === "history";
  const mobilePaneLabels: Record<MobilePane, string> = {
    commit: t("app.commitFiles"),
    editor: t("app.editor"),
    sidebar: t("app.sidebar"),
    terminal: t("app.terminal"),
  };
  const mobilePanes: MobilePane[] = historyReviewOpen
    ? ["sidebar", "commit", "editor", "terminal"]
    : ["sidebar", "editor", "terminal"];

  return (
    <main className="app-shell">
      <header className="app-header">
        <div className="brand">
          <span className="brand-mark" aria-hidden="true">
            G
          </span>
          <div>
            <strong>Graft Playground</strong>
            <small>{t("brand.subtitle")}</small>
          </div>
        </div>
        <div className="header-context">
          <div className="runtime-strip" aria-live="polite">
            <span className={runtimeReady ? "status-dot is-ready" : "status-dot"} />
            <span>{message}</span>
          </div>
          <div className="header-worktree-summary" aria-label={t("tree.storageLegend")}>
            <span className="header-worktree-meta">
              <strong>{t("tree.worktreeRoot")}</strong>
              <small>
                {t("app.fileCount", {
                  count: opfsFileCount,
                  size: (totalBytes / 1024 / 1024).toFixed(1),
                })}
              </small>
            </span>
            <span className="header-storage-state" title={t("tree.vfsTitle")}>
              <b>VFS</b>
              <em>{t("tree.vfsShort")}</em>
            </span>
            <span className="header-storage-state" title={t("tree.materializedTitle")}>
              <b>OPFS</b>
              <em>{t("tree.materializedShort")}</em>
            </span>
          </div>
        </div>
        <div className="header-actions">
          <div className="capabilities" aria-label={t("app.runtimeCapabilities")}>
            <span>WASM</span>
            <span>WORKER</span>
            <span>OPFS</span>
          </div>
          <div className="language-switcher" role="group" aria-label={t("language.label")}>
            <button
              aria-label={t("language.chinese")}
              aria-pressed={language === "zh-CN"}
              onClick={() => setLanguage("zh-CN")}
              title={t("language.chinese")}
              type="button"
            >
              中
            </button>
            <button
              aria-label={t("language.english")}
              aria-pressed={language === "en"}
              onClick={() => setLanguage("en")}
              title={t("language.english")}
              type="button"
            >
              EN
            </button>
          </div>
          <button
            aria-pressed={guideOpen}
            className="utility-button"
            onClick={() => setGuideOpen((current) => !current)}
            type="button"
          >
            {guideOpen ? t("app.hideGuide") : t("app.showGuide")}
          </button>
          <button
            className="utility-button is-danger"
            disabled={busy}
            onClick={() => setResetOpen(true)}
            type="button"
          >
            {t("app.resetData")}
          </button>
        </div>
      </header>

      <nav className="mobile-nav" aria-label={t("app.workbenchAreas")}>
        {mobilePanes.map((pane) => (
          <button
            aria-current={mobilePane === pane ? "page" : undefined}
            key={pane}
            onClick={() => {
              if (pane === "terminal") setTerminalOpen(true);
              setMobilePane(pane);
            }}
            type="button"
          >
            {mobilePaneLabels[pane]}
          </button>
        ))}
      </nav>

      <div
        className={`ide-workspace mobile-${mobilePane} ${historyReviewOpen ? "has-commit-files" : ""} ${guideOpen ? "has-guide" : ""}`}
        style={
          {
            "--commit-files-width": `${commitFilesWidth}px`,
            "--explorer-width": `${explorerWidth}px`,
            "--guide-width": `${guideWidth}px`,
            "--terminal-height": `${terminalHeight}px`,
          } as CSSProperties
        }
      >
        <aside className={`ide-sidebar is-${sidebarTab}`} data-area="sidebar">
          <div className="sidebar-tabs" role="tablist">
            <button
              aria-selected={sidebarTab === "files"}
              onClick={() => setSidebarTab("files")}
              role="tab"
              type="button"
            >
              {t("app.files")}
            </button>
            <button
              aria-selected={sidebarTab === "version"}
              onClick={() => setSidebarTab("version")}
              role="tab"
              type="button"
            >
              {t("app.version")}
              {status.paths.length > 0 && <span>{status.paths.length}</span>}
            </button>
          </div>

          {sidebarTab === "files" ? (
            <div className="sidebar-body explorer-body">
              <ExplorerPane
                entries={entries}
                onSelect={openFile}
                selectedPath={selectedFilePath}
                statusPaths={status.paths}
                trackedPaths={trackedPaths}
              />
            </div>
          ) : (
            <>
              <div className="sidebar-context">
                <strong>
                  {repositoryReady
                    ? (status.current_branch ?? t("app.detached"))
                    : t("app.version")}
                </strong>
                <small className={status.work_in_progress ? "dirty" : "clean"}>
                  {repositoryReady
                    ? status.work_in_progress
                      ? t("app.workInProgress")
                      : t("app.clean")
                    : t("app.noRepository")}
                </small>
              </div>
              <div className="sidebar-body versions-body">
                {repositoryReady ? (
                  <VersionPanel
                    activeTab={versionTab}
                    branches={branches}
                    busy={busy}
                    history={history}
                    onCommit={(commitMessage) =>
                      runGuiCommand(
                        ["commit", "--json", "-m", commitMessage],
                        t("status.commitCreated"),
                      )
                    }
                    onCreateBranch={(name) =>
                      runGuiCommand(
                        ["switch", "--json", "-c", name],
                        t("status.branchCreated", { name }),
                      )
                    }
                    onDiscardPaths={discardWorktreePaths}
                    onMergeBranch={mergeBranch}
                    onReset={resetVersion}
                    onSelectHistory={(commit, path) => void loadHistoryDiff(commit, path)}
                    onSelectPath={(path) => void loadCurrentDiff(path)}
                    onStageAll={() =>
                      runGuiCommand(
                        ["add", "--json", "--all"],
                        t("status.stagedAll"),
                      )
                    }
                    onStagePath={(path) =>
                      runGuiCommand(
                        ["add", "--json", path],
                        t("status.stagedPath", { path }),
                      )
                    }
                    onSwitchBranch={(branch) =>
                      runGuiCommand(
                        ["switch", "--json", branch],
                        t("status.switchedBranch", { name: branch }),
                      )
                    }
                    onTabChange={(tab) => {
                      setVersionTab(tab);
                      if (tab === "changes" && mobilePane === "commit") {
                        setMobilePane("sidebar");
                      } else if (tab === "history" && !selectedHistoryId && history[0]) {
                        void loadHistoryDiff(history[0]);
                      }
                    }}
                    onUnstageAll={() =>
                      runGuiCommand(
                        ["restore", "--json", "--staged", "--all"],
                        t("status.unstagedAll"),
                      )
                    }
                    onUnstagePath={(path) =>
                      runGuiCommand(
                        ["restore", "--json", "--staged", path],
                        t("status.unstagedPath", { path }),
                      )
                    }
                    selectedHistoryId={selectedHistoryId}
                    selectedPath={selectedVersionPath}
                    status={status}
                  />
                ) : (
                  <div className="sidebar-empty">
                    <strong>{t("app.noRepositoryTitle")}</strong>
                    <p>{t("app.noRepositoryBody")}</p>
                  </div>
                )}
              </div>
            </>
          )}
        </aside>

        <ResizeHandle
          axis="vertical"
          label={t("app.resizeFiles")}
          onDelta={(delta) =>
            setExplorerWidth((current) => clamp(current + delta, 190, 480))
          }
        />

        {historyReviewOpen && (
          <>
            <CommitFilesPane
              commit={selectedHistoryCommit}
              onSelectPath={(commit, path) => void loadHistoryDiff(commit, path)}
              selectedPath={selectedVersionPath}
            />
            <ResizeHandle
              axis="vertical"
              label={t("app.resizeCommitFiles")}
              onDelta={(delta) =>
                setCommitFilesWidth((current) => clamp(current + delta, 210, 420))
              }
            />
          </>
        )}

        <div className={`ide-main ${terminalOpen ? "" : "is-terminal-closed"}`}>
          <div className="primary-surface" data-area="editor">
            {surface.type === "conflict" ? (
              <ConflictResolver
                busy={busy}
                conflicts={conflicts}
                onAbort={abortMerge}
                onContinue={continueMerge}
                onResolve={resolveConflict}
                onSelectPath={(path) => {
                  setSelectedVersionPath(path);
                  setSurface({ path, type: "conflict" });
                }}
                selectedPath={surface.path || selectedVersionPath}
              />
            ) : surface.type === "file" ? (
              <EditorPane
                key={`file:${surface.path}:${surfaceEpoch}`}
                modified={selectedEntry?.modified}
                onSave={saveFile}
                onUploadAttachments={uploadAttachments}
                path={surface.path}
              />
            ) : surface.type === "sqlite" ? (
              <SqliteEditor
                client={client}
                key={`sqlite:${surface.path}:${surfaceEpoch}`}
                materialized={Boolean(selectedEntry)}
                onChanged={sqliteChanged}
                path={surface.path}
                workspaceVersion={workspaceVersion}
              />
            ) : surface.type === "diff" ? (
              surface.diff.kind === "sqlite_database" ? (
                <SqliteDiffPane diff={surface.diff} />
              ) : surface.diff.kind === "binary_file" ? (
                <BinaryDiffPane diff={surface.diff} />
              ) : (
                <DiffPane diff={surface.diff} />
              )
            ) : (
              <section className="welcome-surface">
                <div className="welcome-mark" aria-hidden="true">
                  G
                </div>
                <h1>{t("app.welcomeTitle")}</h1>
                <p>{t("app.welcomeBody")}</p>
                {!repositoryReady && runtimeReady && (
                  <button disabled={busy} onClick={() => void createDemo()} type="button">
                    {busy ? t("app.creatingDemo") : t("app.createDemo")}
                  </button>
                )}
                <dl>
                  <div>
                    <dt>{t("app.files")}</dt>
                    <dd>{t("app.filesDescription")}</dd>
                  </div>
                  <div>
                    <dt>{t("app.version")}</dt>
                    <dd>{t("app.versionDescription")}</dd>
                  </div>
                  <div>
                    <dt>{t("app.terminal")}</dt>
                    <dd>{t("app.terminalDescription")}</dd>
                  </div>
                </dl>
              </section>
            )}
          </div>

          <ResizeHandle
            axis="horizontal"
            className="terminal-resize-handle"
            label={t("app.resizeTerminal")}
            onDelta={(delta) =>
              setTerminalHeight((current) => clamp(current - delta, 150, 480))
            }
          />

          <section className="terminal-dock" data-area="terminal">
            <header className="dock-header">
              <div>
                <span>{t("app.terminal").toUpperCase()}</span>
                <strong>OPFS shell · graft.wasm</strong>
              </div>
              <div className="dock-actions">
                {!repositoryReady && runtimeReady && (
                  <button disabled={busy} onClick={() => void createDemo()} type="button">
                    {busy ? t("app.creating") : t("app.createDemoShort")}
                  </button>
                )}
                <button
                  aria-label={t("app.closeTerminal", { shortcut: terminalShortcut() })}
                  className="terminal-toggle-button"
                  onClick={closeTerminal}
                  title={t("app.closeTerminal", { shortcut: terminalShortcut() })}
                  type="button"
                >
                  ×
                </button>
              </div>
            </header>
            <div className="terminal-body">
              <TerminalPanel
                client={client}
                key={terminalEpoch}
                onCommandComplete={refresh}
                onOpenFile={openFile}
                ref={terminalRef}
                runtimeReady={runtimeReady}
              />
            </div>
          </section>

          <button
            aria-label={t("app.openTerminal", { shortcut: terminalShortcut() })}
            className="terminal-collapsed-bar"
            onClick={openTerminal}
            title={t("app.openTerminal", { shortcut: terminalShortcut() })}
            type="button"
          >
            <span>{t("app.terminal").toUpperCase()}</span>
            <strong>OPFS shell · graft.wasm</strong>
            <kbd>{terminalShortcut()}</kbd>
          </button>
        </div>

        {guideOpen && (
          <>
            <ResizeHandle
              axis="vertical"
              label={t("app.resizeGuide")}
              onDelta={(delta) =>
                setGuideWidth((current) => clamp(current - delta, 280, 540))
              }
            />
            <QuickstartGuide
              completed={guideProgress}
              onClose={() => setGuideOpen(false)}
              onResetProgress={() => setGuideProgress([])}
              onRunAction={runGuideAction}
              onRunCommand={runGuideCommand}
              onToggle={toggleGuideStep}
            />
          </>
        )}
      </div>

      {resetOpen && (
        <ResetDialog
          busy={busy}
          onCancel={() => setResetOpen(false)}
          onConfirm={() => void resetAllData()}
        />
      )}
    </main>
  );
}
