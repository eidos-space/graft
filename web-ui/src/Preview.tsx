import { lazy, Suspense, useState } from "react";
import { previewUrl } from "./api";
import type { PreviewKind } from "./previewKind";
export { previewKind } from "./previewKind";

const PdfPreview = lazy(() => import("./PdfPreview"));

export function Preview({ path, kind }: { path: string; kind: PreviewKind }) {
  const [error, setError] = useState(false);
  const url = previewUrl(path);
  return (
    <div className={`file-preview file-preview-${kind}`}>
      <div className="preview-content">
        {error ? (
          <div className="empty" role="status">
            This file could not be previewed. It may be missing or use an unsupported codec.
          </div>
        ) : kind === "image" ? (
          <img src={url} alt={path} onError={() => setError(true)} />
        ) : kind === "audio" ? (
          <audio src={url} controls preload="metadata" onError={() => setError(true)} />
        ) : kind === "video" ? (
          <video src={url} controls preload="metadata" onError={() => setError(true)} />
        ) : kind === "pdf" ? (
          <Suspense
            fallback={
              <div className="empty" role="status">
                Loading PDF…
              </div>
            }
          >
            <PdfPreview url={url} />
          </Suspense>
        ) : null}
      </div>
    </div>
  );
}
