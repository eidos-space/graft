import { useI18n } from "../i18n";
import type { BinaryDiffView } from "../types";

function formatSize(bytes: number | undefined, locale: string, unavailable: string) {
  if (bytes === undefined) return unavailable;
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = units[0];
  for (let index = 1; value >= 1024 && index < units.length; index += 1) {
    value /= 1024;
    unit = units[index];
  }
  return `${new Intl.NumberFormat(locale, { maximumFractionDigits: 1 }).format(value)} ${unit}`;
}

export function BinaryDiffPane({ diff }: { diff: BinaryDiffView }) {
  const { locale, t } = useI18n();
  const fileName = diff.path.split("/").at(-1) ?? diff.path;

  return (
    <section className="binary-diff-surface" aria-label={t("binaryDiff.label", { path: diff.path })}>
      <header className="surface-tabbar">
        <div className="surface-file-tab is-diff is-binary">
          <span className="file-glyph" aria-hidden="true">
            ◇
          </span>
          <strong>{diff.path}</strong>
        </div>
        <div className="surface-actions">
          <span>{t("binaryDiff.change")}</span>
        </div>
      </header>

      <div className="binary-diff-content">
        <div className="binary-diff-heading">
          <span>{t("binaryDiff.eyebrow")}</span>
          <h1>{fileName}</h1>
          <p>{t("binaryDiff.description")}</p>
        </div>
        <dl>
          <div>
            <dt>{t("binaryDiff.path")}</dt>
            <dd><code>/{diff.path}</code></dd>
          </div>
          <div>
            <dt>{t("binaryDiff.operation")}</dt>
            <dd>{t(`version.change.${diff.change}`)}</dd>
          </div>
          <div>
            <dt>{t("binaryDiff.size")}</dt>
            <dd>{formatSize(diff.size, locale, t("binaryDiff.unavailable"))}</dd>
          </div>
          <div>
            <dt>{t("binaryDiff.storage")}</dt>
            <dd><code>{diff.storage}</code></dd>
          </div>
        </dl>
        {diff.description && <p className="binary-diff-revision">{diff.description}</p>}
      </div>

      <footer className="surface-statusbar">
        <span>{t("version.kind.binary_file")}</span>
        <span>{t("binaryDiff.managed")}</span>
      </footer>
    </section>
  );
}
