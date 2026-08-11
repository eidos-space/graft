import { parseDiffFromFile } from "@pierre/diffs";
import { FileDiff, Virtualizer } from "@pierre/diffs/react";
import { useMemo, useState } from "react";
import { useI18n } from "../i18n";
import type { TextDiffView } from "../types";
import { DiffInspectorHeader } from "./DiffInspectorHeader";

function byteCount(value: string, locale: string) {
  return new Intl.NumberFormat(locale, { notation: "compact" }).format(
    new TextEncoder().encode(value).byteLength,
  );
}

export function DiffPane({ diff, onClose }: { diff: TextDiffView; onClose: () => void }) {
  const { locale, t } = useI18n();
  const [layout, setLayout] = useState<"split" | "unified">("split");
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
    <section
      className="diff-surface version-inspector"
      aria-label={t("diff.label", { path: diff.path })}
    >
      <DiffInspectorHeader
        mode={diff.label?.startsWith("HISTORY") ? t("version.history") : t("version.changes")}
        onClose={onClose}
        path={diff.path}
      />
      <div className="version-text-diff">
        <header className="diff-inspector-toolbar version-text-diff-toolbar">
          <div>
            <strong>{t("diff.textChanges")}</strong>
            <span>
              {byteCount(diff.before, locale)} B
              <i aria-hidden="true"> → </i>
              {byteCount(diff.after, locale)} B
              <i aria-hidden="true"> · </i>
              {label}
              <i aria-hidden="true"> · </i>
              {description}
            </span>
          </div>
          <div className="version-text-diff-layout" aria-label={t("diff.layout")}>
            <button
              aria-pressed={layout === "split"}
              onClick={() => setLayout("split")}
              type="button"
            >
              {t("diff.split")}
            </button>
            <button
              aria-pressed={layout === "unified"}
              onClick={() => setLayout("unified")}
              type="button"
            >
              {t("diff.unified")}
            </button>
          </div>
        </header>
        <div className="diff-editor-scroll version-text-diff-surface">
          <Virtualizer className="version-text-diff-virtualizer">
            <FileDiff
              fileDiff={fileDiff}
              options={{
                diffStyle: layout,
                disableFileHeader: true,
                hunkSeparators: "simple",
                lineDiffType: "word",
                overflow: "scroll",
                stickyHeader: false,
                theme: "pierre-light",
                themeType: "light",
              }}
            />
          </Virtualizer>
        </div>
      </div>
    </section>
  );
}
