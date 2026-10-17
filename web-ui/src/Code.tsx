import { parseDiffFromFile } from "@pierre/diffs";
import { EditProvider, File, FileDiff, Virtualizer, type EditorFactory } from "@pierre/diffs/react";
import { Editor } from "@pierre/diffs/edit";
import { useMemo } from "react";
import type { FilePair } from "./types";
import { useTheme } from "./theme";

export interface DiffPreferences {
  wrap: boolean;
  layout: "unified" | "split";
}

const createEditor: EditorFactory<undefined, undefined> = (type, options, key) =>
  new Editor(type, options, key);

const options = {
  disableFileHeader: true,
  theme: { dark: "pierre-dark", light: "pierre-light" } as const,
  overflow: "scroll" as const,
  unsafeCSS: `:host { --diffs-font-size: 13px; --diffs-line-height: 23px; --diffs-font-family: "SFMono-Regular", Consolas, "Liberation Mono", monospace; }
    [data-code], [data-gutter] { font-size: 13px; line-height: 23px; }`,
};

export function Code({
  pair,
  path,
  compare,
  editing,
  onDraft,
  preferences,
  expandUnchanged = false,
}: {
  pair: FilePair;
  path: string;
  compare: boolean;
  editing: boolean;
  onDraft: (content: string) => void;
  preferences: DiffPreferences;
  expandUnchanged?: boolean;
}) {
  const theme = useTheme();
  const themedOptions = useMemo(() => ({ ...options, themeType: theme }), [theme]);
  const before = pair.before.content;
  const after = pair.after.content;
  const file = useMemo(
    () => ({
      name: path,
      contents: after ?? "",
      cacheKey: `${path}:${pair.after.etag ?? pair.after.content_hash ?? after}`,
    }),
    [path, after, pair.after.etag, pair.after.content_hash],
  );
  const diff = useMemo(() => {
    if (!compare || (pair.before.state === "absent" && pair.after.state === "absent"))
      return undefined;
    return parseDiffFromFile(
      pair.before.state === "absent" ? null : { name: path, contents: before ?? "" },
      pair.after.state === "absent" ? null : { name: path, contents: after ?? "" },
    );
  }, [compare, path, before, after, pair.before.state, pair.after.state]);

  const unavailable = [pair.before, pair.after].find(
    (content) => content.state !== "utf8" && content.state !== "absent",
  );
  if (unavailable && (compare || pair.after !== unavailable)) {
    return (
      <div className="empty">
        {unavailable.state === "too_large"
          ? "This file exceeds the 2 MB preview limit."
          : unavailable.state === "missing_payload"
            ? "This version is not available locally."
            : "Binary file · no text preview"}
      </div>
    );
  }
  if (!compare && pair.after.state !== "utf8")
    return (
      <div className="empty">
        {pair.after.state === "absent"
          ? "This file is no longer in the worktree."
          : "No text preview available."}
      </div>
    );
  if (compare && before === after && !expandUnchanged)
    return <div className="empty">No content changes</div>;
  return (
    <EditProvider createEditor={createEditor}>
      <Virtualizer className="code-scroll">
        {compare && diff && before !== after ? (
          <FileDiff
            fileDiff={diff}
            options={{
              ...themedOptions,
              diffStyle: preferences.layout,
              overflow: preferences.wrap ? "wrap" : "scroll",
              hunkSeparators: "line-info",
              lineDiffType: "word",
              stickyHeader: false,
              expandUnchanged,
            }}
          />
        ) : (
          <File
            file={file}
            edit={editing}
            options={{ ...themedOptions, overflow: preferences.wrap ? "wrap" : "scroll" }}
            onEditChange={(event) => onDraft(event.file.contents)}
            onEditComplete={() => "reject"}
          />
        )}
      </Virtualizer>
    </EditProvider>
  );
}
