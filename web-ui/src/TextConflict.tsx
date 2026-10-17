import { UnresolvedFile, type FileContents, type UnresolvedFileOptions } from "@pierre/diffs";
import { useLayoutEffect, useMemo, useRef, useState } from "react";
import { api } from "./api";
import { useTheme } from "./theme";
import { prepareTextMerge } from "./textMerge";
import type { DiffPreferences } from "./Code";
import { useReviewDraft } from "./useReviewDraft";
import { clearReviewDraft } from "./reviewDrafts";
import { useReviewBusy } from "./useReviewBusy";

export function TextConflict({
  path,
  base,
  current,
  incoming,
  token,
  expected,
  preferences,
  onChanged,
  draftScope,
}: {
  path: string;
  base: string;
  current: string;
  incoming: string;
  token: string;
  expected?: string;
  preferences: DiffPreferences;
  onChanged: () => void;
  draftScope: string;
}) {
  const initial = useMemo(
    () => prepareTextMerge(base, current, incoming),
    [base, current, incoming],
  );
  const draftKey = JSON.stringify([draftScope, path, "text", expected]);
  const [review, setReview] = useReviewDraft(draftKey, {
    draft: initial.contents,
    remaining: initial.conflicts,
  });
  const { draft, remaining } = review;
  const [busy, setBusy] = useState(false);
  useReviewBusy(busy);
  const [error, setError] = useState("");
  const host = useRef<HTMLDivElement>(null);
  const instance = useRef<UnresolvedFile | undefined>(undefined);
  const theme = useTheme();
  const options = useMemo<UnresolvedFileOptions<undefined>>(
    () => ({
      disableFileHeader: true,
      theme: { dark: "pierre-dark", light: "pierre-light" },
      themeType: theme,
      overflow: preferences.wrap ? "wrap" : "scroll",
      mergeConflictActionsType: "default",
      unsafeCSS:
        ':host { --diffs-font-size: 13px; --diffs-line-height: 23px; --diffs-font-family: "SFMono-Regular", Consolas, monospace; }',
      onMergeConflictResolve: (file: FileContents) => {
        setReview((previous) => ({
          draft: file.contents,
          remaining: Math.max(0, previous.remaining - 1),
        }));
      },
    }),
    [theme, preferences.wrap, setReview],
  );
  const optionsRef = useRef(options);
  optionsRef.current = options;
  useLayoutEffect(() => {
    if (!host.current) return;
    const view = new UnresolvedFile(optionsRef.current);
    instance.current = view;
    view.render({
      file: { name: path, contents: draft },
      containerWrapper: host.current,
    });
    return () => {
      view.cleanUp();
      instance.current = undefined;
    };
  }, [path, initial]);
  useLayoutEffect(() => {
    instance.current?.setOptions(options);
    instance.current?.rerender();
  }, [options]);
  const save = async () => {
    if (remaining || busy) return;
    setBusy(true);
    setError("");
    try {
      const content = initial.trimFinalNewline ? draft.replace(/\r?\n$/, "") : draft;
      await api("resolve", { path, state_token: token, action: "edited", content, expected });
      clearReviewDraft(draftKey);
      onChanged();
    } catch (error) {
      setError(error instanceof Error ? error.message : "Could not save resolution");
    } finally {
      setBusy(false);
    }
  };
  return (
    <>
      <div className="conflict-actions">
        <span role="status">
          {remaining
            ? `${remaining} unresolved ${remaining === 1 ? "block" : "blocks"}`
            : "Ready to save"}
        </span>
        <button disabled={remaining > 0 || busy} onClick={() => void save()}>
          {busy ? "Saving…" : "Save and mark resolved"}
        </button>
      </div>
      {error && (
        <div className="notice" role="alert">
          {error}
        </div>
      )}
      <div className="code-scroll" ref={host} />
    </>
  );
}
