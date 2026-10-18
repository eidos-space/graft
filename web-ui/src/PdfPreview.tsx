import { useEffect, useRef, useState } from "react";
import { getDocument, GlobalWorkerOptions, type PDFDocumentProxy } from "pdfjs-dist";
import workerUrl from "pdfjs-dist/build/pdf.worker.min.mjs?url";

GlobalWorkerOptions.workerSrc = workerUrl;

export default function PdfPreview({ url }: { url: string }) {
  const [document, setDocument] = useState<PDFDocumentProxy>();
  const [pageNumber, setPageNumber] = useState(1);
  const [width, setWidth] = useState(600);
  const [error, setError] = useState("");
  const [rendering, setRendering] = useState(true);
  const container = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  useEffect(() => {
    let disposed = false;
    setError("");
    const task = getDocument({ url, useSystemFonts: true, useWasm: false });
    void task.promise
      .then((next) => {
        if (!disposed) setDocument(next);
      })
      .catch((error: unknown) => {
        if (!disposed) setError(error instanceof Error ? error.message : "Could not open PDF");
      });
    return () => {
      disposed = true;
      void task.destroy();
    };
  }, [url]);
  useEffect(() => {
    const element = container.current;
    if (!element) return;
    const observer = new ResizeObserver(([entry]) => {
      setWidth(Math.max(1, Math.floor(entry.contentRect.width - 48)));
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);
  useEffect(() => {
    if (!document || !canvas.current) return;
    let disposed = false;
    let cancel: (() => void) | undefined;
    const element = canvas.current;
    setRendering(true);
    void document
      .getPage(pageNumber)
      .then(async (page) => {
        if (disposed) return;
        const natural = page.getViewport({ scale: 1 });
        const viewport = page.getViewport({
          scale: Math.min(width, natural.width) / natural.width,
        });
        const ratio = Math.min(window.devicePixelRatio || 1, 2);
        element.width = Math.ceil(viewport.width * ratio);
        element.height = Math.ceil(viewport.height * ratio);
        element.style.width = `${viewport.width}px`;
        element.style.height = `${viewport.height}px`;
        const task = page.render({
          canvas: element,
          viewport,
          transform: [ratio, 0, 0, ratio, 0, 0],
        });
        cancel = () => task.cancel();
        await task.promise;
        if (!disposed) setRendering(false);
      })
      .catch((error: unknown) => {
        if (!disposed) setError(error instanceof Error ? error.message : "Could not render PDF");
      });
    return () => {
      disposed = true;
      cancel?.();
    };
  }, [document, pageNumber, width]);
  return (
    <div className="pdf-preview">
      <div className="pdf-toolbar">
        <button
          disabled={!document || pageNumber === 1}
          onClick={() => setPageNumber((page) => page - 1)}
          aria-label="Previous page"
        >
          ←
        </button>
        <span>{document ? `${pageNumber} / ${document.numPages}` : "Loading PDF…"}</span>
        <button
          disabled={!document || pageNumber === document.numPages}
          onClick={() => setPageNumber((page) => page + 1)}
          aria-label="Next page"
        >
          →
        </button>
      </div>
      <div className="pdf-pages" ref={container}>
        {error ? (
          <div className="empty" role="status">
            {error}
          </div>
        ) : (
          <canvas ref={canvas} aria-label={`PDF page ${pageNumber}`} aria-busy={rendering} />
        )}
      </div>
    </div>
  );
}
