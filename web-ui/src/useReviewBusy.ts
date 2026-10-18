import { useLayoutEffect } from "react";

const operations = new Set<symbol>();

/** Repository writes must finish before their view can be unmounted. */
export function useReviewBusy(busy: boolean) {
  useLayoutEffect(() => {
    if (!busy) return;
    const operation = Symbol();
    operations.add(operation);
    document.body.dataset.reviewSaving = "true";
    window.dispatchEvent(new Event("graft-review-saving"));
    const prevent = (event: BeforeUnloadEvent) => event.preventDefault();
    window.addEventListener("beforeunload", prevent);
    return () => {
      operations.delete(operation);
      if (!operations.size) delete document.body.dataset.reviewSaving;
      window.dispatchEvent(new Event("graft-review-saving"));
      window.removeEventListener("beforeunload", prevent);
    };
  }, [busy]);
}
