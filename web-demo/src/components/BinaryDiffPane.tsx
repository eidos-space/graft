import { useI18n } from "../i18n";
import type { BinaryContentState, BinaryDiffView } from "../types";

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

function imageMimeType(path: string) {
  const extension = path.split(".").at(-1)?.toLowerCase();
  if (extension === "avif") return "image/avif";
  if (extension === "bmp") return "image/bmp";
  if (extension === "gif") return "image/gif";
  if (extension === "ico") return "image/x-icon";
  if (extension === "jpg" || extension === "jpeg") return "image/jpeg";
  if (extension === "png") return "image/png";
  if (extension === "webp") return "image/webp";
  return undefined;
}

function ImageRevision({
  fileName,
  label,
  locale,
  mimeType,
  state,
}: {
  fileName: string;
  label: string;
  locale: string;
  mimeType: string;
  state: BinaryContentState | undefined;
}) {
  const { t } = useI18n();
  const unavailable = !state
    ? t("binaryDiff.state.unavailable")
    : state.state === "absent"
      ? t("binaryDiff.state.absent")
      : state.state === "too_large"
        ? t("binaryDiff.state.too_large")
        : state.state === "missing_payload"
          ? t("binaryDiff.state.missing_payload")
          : state.state === "utf8"
            ? t("binaryDiff.state.utf8")
            : t("binaryDiff.state.invalid_utf8");
  return (
    <figure className={`binary-image-revision is-${state?.state ?? "unavailable"}`}>
      <figcaption>
        <strong>{label}</strong>
        <span>{formatSize(state?.size, locale, t("binaryDiff.unavailable"))}</span>
      </figcaption>
      <div>
        {state?.state === "base64" && state.content ? (
          <img
            alt={t("binaryDiff.imageAlt", { file: fileName, revision: label })}
            src={`data:${mimeType};base64,${state.content}`}
          />
        ) : (
          <span>{unavailable}</span>
        )}
      </div>
    </figure>
  );
}

export function BinaryDiffPane({ diff }: { diff: BinaryDiffView }) {
  const { locale, t } = useI18n();
  const fileName = diff.path.split("/").at(-1) ?? diff.path;
  const mimeType = imageMimeType(diff.path);
  const hasImageHistory = Boolean(mimeType && (diff.before || diff.after));

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

      <div className={`binary-diff-content ${hasImageHistory ? "has-image-history" : ""}`}>
        <div className="binary-diff-heading">
          <span>{t(hasImageHistory ? "binaryDiff.imageEyebrow" : "binaryDiff.eyebrow")}</span>
          <h1>{fileName}</h1>
          <p>{t(hasImageHistory ? "binaryDiff.imageDescription" : "binaryDiff.description")}</p>
        </div>
        {hasImageHistory && mimeType && (
          <div className="binary-image-history">
            <ImageRevision
              fileName={fileName}
              label={t("binaryDiff.before")}
              locale={locale}
              mimeType={mimeType}
              state={diff.before}
            />
            <ImageRevision
              fileName={fileName}
              label={t("binaryDiff.after")}
              locale={locale}
              mimeType={mimeType}
              state={diff.after}
            />
          </div>
        )}
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
