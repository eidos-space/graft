import { useI18n } from "../i18n";

export function DiffInspectorHeader({
  mode,
  onClose,
  path,
}: {
  mode: string;
  onClose: () => void;
  path: string;
}) {
  const { t } = useI18n();
  const fileName = path.split("/").at(-1) ?? path;

  return (
    <header className="diff-inspector-header">
      <div className="diff-inspector-breadcrumb" title={path}>
        <span>{mode}</span>
        <i aria-hidden="true">›</i>
        <strong>{fileName}</strong>
      </div>
      <button
        aria-label={t("diff.close", { path })}
        onClick={onClose}
        title={t("diff.close", { path })}
        type="button"
      >
        <span aria-hidden="true">×</span>
      </button>
    </header>
  );
}
