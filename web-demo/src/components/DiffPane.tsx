import { parseDiffFromFile } from "@pierre/diffs";
import { FileDiff } from "@pierre/diffs/react";
import { useMemo } from "react";
import { useI18n } from "../i18n";
import type { TextDiffView } from "../types";

export function DiffPane({ diff }: { diff: TextDiffView }) {
  const { t } = useI18n();
  const label =
    diff.label === "HISTORY DIFF"
      ? t("diff.history")
      : diff.label === "WORKTREE DIFF"
        ? t("diff.worktree")
        : (diff.label ?? t("diff.worktree"));
  const description = (() => {
    if (!diff.description) return t("diff.comparing");
    if (diff.description === "Empty tree → worktree") return t("diff.emptyToWorktree");
    if (diff.description.endsWith(" → worktree")) {
      return `${diff.description.slice(0, -"worktree".length)}${t("diff.worktreeState")}`;
    }
    if (diff.description.startsWith("empty → ")) {
      return `${t("diff.emptyState")} → ${diff.description.slice("empty → ".length)}`;
    }
    return diff.description;
  })();
  const fileDiff = useMemo(
    () =>
      parseDiffFromFile(
        {
          cacheKey: `before:${diff.path}:${diff.before.length}`,
          contents: diff.before,
          name: diff.path,
        },
        {
          cacheKey: `after:${diff.path}:${diff.after.length}`,
          contents: diff.after,
          name: diff.path,
        },
      ),
    [diff],
  );

  return (
    <section className="diff-surface" aria-label={t("diff.label", { path: diff.path })}>
      <header className="surface-tabbar">
        <div className="surface-file-tab is-diff">
          <span className="file-glyph" aria-hidden="true">
            ±
          </span>
          <strong>{diff.path}</strong>
        </div>
        <div className="surface-actions">
          <span>{label}</span>
        </div>
      </header>
      <div className="diff-editor-scroll">
        <FileDiff
          fileDiff={fileDiff}
          options={{
            diffStyle: "split",
            hunkSeparators: "simple",
            lineDiffType: "word",
            overflow: "wrap",
            stickyHeader: true,
            theme: "pierre-light",
            themeType: "light",
          }}
        />
      </div>
      <footer className="surface-statusbar">
        <span>{diff.kind.replaceAll("_", " ")}</span>
        <span>{description}</span>
      </footer>
    </section>
  );
}
