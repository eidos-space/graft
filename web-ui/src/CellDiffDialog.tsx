import { lazy, Suspense, useLayoutEffect, useId, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import type { DiffPreferences } from "./Code";

const Code = lazy(() => import("./Code").then((module) => ({ default: module.Code })));

export interface CellComparison {
  mode?: "value";
  column: string;
  row: string;
  before?: string;
  after?: string;
}

/** Cell detail uses the file diff renderer, with optional conflict choices. */
export function CellDiffDialog({
  comparison,
  onClose,
  resolution,
  loading = false,
  error,
}: {
  comparison: CellComparison;
  onClose: () => void;
  loading?: boolean;
  error?: string;
  resolution?: {
    selected?: "ours" | "theirs";
    disabled?: boolean;
    onSelect: (side: "ours" | "theirs") => void;
  };
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const opener = useRef(document.activeElement as HTMLElement | null);
  const title = useId();
  const [preferences, setPreferences] = useState<DiffPreferences>({
    wrap: true,
    layout: "split",
  });
  const pair = useMemo(
    () => ({
      before:
        comparison.before === undefined
          ? { state: "absent" as const }
          : { state: "utf8" as const, content: comparison.before },
      after:
        comparison.after === undefined
          ? { state: "absent" as const }
          : { state: "utf8" as const, content: comparison.after },
    }),
    [comparison],
  );
  useLayoutEffect(() => {
    const node = dialog.current;
    node?.showModal();
    node?.querySelector<HTMLButtonElement>(".cell-diff-close")?.focus({ preventScroll: true });
    return () => {
      node?.close();
      if (opener.current?.isConnected) opener.current.focus({ preventScroll: true });
    };
  }, []);
  return createPortal(
    <dialog
      ref={dialog}
      className="cell-diff-dialog"
      aria-labelledby={title}
      onCancel={(event) => {
        event.preventDefault();
        onClose();
      }}
    >
      <div className="cell-diff-shell">
        <header className="cell-diff-header">
          <div className="cell-diff-title">
            <h2 id={title}>{comparison.column}</h2>
            <span className="subtle">Row {comparison.row.replace(/^rowid:/, "")}</span>
          </div>
          <div className="diff-controls" role="group" aria-label="Cell diff display options">
            <button
              aria-label="Wrap"
              aria-pressed={preferences.wrap}
              onClick={() =>
                setPreferences((previous) => ({
                  ...previous,
                  wrap: !previous.wrap,
                }))
              }
            >
              Wrap
            </button>
            {!resolution && comparison.mode !== "value" && (
              <div className="diff-layout" role="group" aria-label="Cell diff layout">
                <button
                  aria-pressed={preferences.layout === "unified"}
                  onClick={() =>
                    setPreferences((previous) => ({
                      ...previous,
                      layout: "unified",
                    }))
                  }
                >
                  Unified
                </button>
                <button
                  aria-pressed={preferences.layout === "split"}
                  onClick={() =>
                    setPreferences((previous) => ({
                      ...previous,
                      layout: "split",
                    }))
                  }
                >
                  Split
                </button>
              </div>
            )}
          </div>
          <button
            className="cell-diff-close"
            aria-label="Close cell comparison"
            onClick={onClose}
            autoFocus
          >
            ×
          </button>
        </header>
        {comparison.mode !== "value" && (
          <div
            className={`cell-diff-legend ${resolution || preferences.layout === "split" ? "split" : ""}`}
          >
            {resolution ? (
              <>
                <button
                  className="cell-conflict-choice"
                  aria-pressed={resolution.selected === "ours"}
                  disabled={resolution.disabled}
                  onClick={() => resolution.onSelect("ours")}
                >
                  {resolution.selected === "ours" ? "●" : "○"} Keep current
                </button>
                <button
                  className="cell-conflict-choice"
                  aria-pressed={resolution.selected === "theirs"}
                  disabled={resolution.disabled}
                  onClick={() => resolution.onSelect("theirs")}
                >
                  {resolution.selected === "theirs" ? "●" : "○"} Keep incoming
                </button>
              </>
            ) : (
              <>
                <span className="removed">− Before</span>
                <span className="added">+ After</span>
              </>
            )}
          </div>
        )}
        {loading ? (
          <div className="empty" role="status">
            Loading complete value…
          </div>
        ) : error ? (
          <div className="notice" role="alert">
            {error}
          </div>
        ) : (
          <Suspense
            fallback={
              <div className="empty" role="status">
                Loading comparison…
              </div>
            }
          >
            <Code
              pair={pair}
              path={`${comparison.column}.txt`}
              compare={comparison.mode !== "value"}
              editing={false}
              onDraft={() => {}}
              preferences={preferences}
              expandUnchanged
            />
          </Suspense>
        )}
      </div>
    </dialog>,
    document.body,
  );
}
