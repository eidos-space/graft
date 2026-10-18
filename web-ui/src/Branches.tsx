import { useEffect, useRef, useState } from "react";
import { api, useResource } from "./api";
import { useReviewBusy } from "./useReviewBusy";

interface BranchesPage {
  current_branch?: string;
  current_head?: string;
  branches: { name: string; target?: string }[];
}
export function Branches({
  onSwitch,
  canSwitch,
  epoch,
  currentBranch,
}: {
  onSwitch: () => void;
  canSwitch: () => boolean;
  epoch: number;
  currentBranch?: string;
}) {
  const [open, setOpen] = useState(false);
  const control = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (!open) return;
    const dismiss = (event: PointerEvent) => {
      if (event.target instanceof Node && !control.current?.contains(event.target)) setOpen(false);
    };
    document.addEventListener("pointerdown", dismiss);
    return () => document.removeEventListener("pointerdown", dismiss);
  }, [open]);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  useReviewBusy(busy);
  const [error, setError] = useState("");
  const [version, setVersion] = useState(0);
  const branches = useResource<BranchesPage>(
    "branches",
    {},
    false,
    false,
    open,
    `${epoch}:${version}`,
  );
  const current =
    currentBranch ??
    branches.data?.current_branch ??
    (branches.data?.current_head ? "Detached HEAD" : "Branches");
  const act = async (action: "create" | "switch", name: string) => {
    if (busy || document.body.dataset.reviewSaving) return;
    if (action === "switch" && !canSwitch()) return;
    setBusy(true);
    setError("");
    try {
      await api("branches", { action, name });
      setVersion((value) => value + 1);
      setName("");
      if (action === "switch") {
        setOpen(false);
        onSwitch();
      }
    } catch (error) {
      setError(error instanceof Error ? error.message : "Branch operation failed");
    } finally {
      setBusy(false);
    }
  };
  return (
    <div
      className="branch-control"
      ref={control}
      onKeyDown={(event) => {
        if (event.key !== "Escape" || !open) return;
        event.preventDefault();
        event.stopPropagation();
        setOpen(false);
        trigger.current?.focus();
      }}
    >
      <button
        ref={trigger}
        className="branch-trigger"
        aria-expanded={open}
        aria-controls="branch-menu"
        onClick={() => setOpen((value) => !value)}
      >
        <span aria-hidden="true">⑂</span>
        <span>{current}</span>
        <span aria-hidden="true">⌃</span>
      </button>
      {open && (
        <div className="branch-menu" id="branch-menu">
          <div className="branch-heading">
            Branches{" "}
            <button aria-label="Close branches" onClick={() => setOpen(false)}>
              ×
            </button>
          </div>
          <div className="branch-list">
            {branches.data?.branches.map((branch) => (
              <button
                key={branch.name}
                disabled={busy || branch.name === branches.data?.current_branch}
                onClick={() => void act("switch", branch.name)}
              >
                <span>{branch.name}</span>
                <span>
                  {branch.name === branches.data?.current_branch
                    ? "Current"
                    : branch.target?.slice(0, 7)}
                </span>
              </button>
            ))}
            {!branches.data && !branches.error && <div role="status">Loading branches…</div>}
          </div>
          <form
            onSubmit={(event) => {
              event.preventDefault();
              void act("create", name.trim());
            }}
          >
            <input
              aria-label="New branch name"
              placeholder="New branch name"
              value={name}
              onChange={(event) => setName(event.target.value)}
              disabled={busy}
            />
            <button disabled={busy || !name.trim()}>Create</button>
          </form>
          {(error || branches.error) && (
            <div className="tree-note" role="alert">
              {error || branches.error}
              {branches.error && <button onClick={branches.retry}>Retry</button>}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
