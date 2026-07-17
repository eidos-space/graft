import { useEffect } from "react";
import { useI18n } from "../i18n";

interface ResetDialogProps {
  busy: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}

export function ResetDialog({ busy, onCancel, onConfirm }: ResetDialogProps) {
  const { t } = useI18n();
  useEffect(() => {
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !busy) onCancel();
    };
    window.addEventListener("keydown", closeOnEscape);
    return () => window.removeEventListener("keydown", closeOnEscape);
  }, [busy, onCancel]);

  return (
    <div className="dialog-backdrop" role="presentation">
      <section aria-labelledby="reset-title" aria-modal="true" className="reset-dialog" role="dialog">
        <span>{t("resetData.eyebrow")}</span>
        <h2 id="reset-title">{t("resetData.title")}</h2>
        <p>
          {t("resetData.bodyBefore")} <code>.graft/</code>
          {t("resetData.separator")}
          {t("resetData.bodyAfter")}
        </p>
        <div>
          <button disabled={busy} onClick={onCancel} type="button">
            {t("resetData.cancel")}
          </button>
          <button className="is-danger" disabled={busy} onClick={onConfirm} type="button">
            {busy ? t("resetData.busy") : t("resetData.confirm")}
          </button>
        </div>
      </section>
    </div>
  );
}
