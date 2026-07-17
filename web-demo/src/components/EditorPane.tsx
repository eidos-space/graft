import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useI18n } from "../i18n";
import { readOpfsFile, readOpfsPreview } from "../lib/opfs";

interface EditorPaneProps {
  modified?: number;
  onSave: (path: string, contents: string) => Promise<void>;
  onUploadAttachments: (files: File[]) => Promise<string[]>;
  path: string;
}

type LoadState = "binary" | "error" | "image" | "loading" | "ready" | "too_large";

const IMAGE_EXTENSIONS = new Set(["avif", "bmp", "gif", "jpeg", "jpg", "png", "webp"]);
const MAX_IMAGE_PREVIEW_BYTES = 20 * 1024 * 1024;

function imageExtension(path: string) {
  const extension = path.split(".").pop()?.toLowerCase();
  return extension && IMAGE_EXTENSIONS.has(extension) ? extension : undefined;
}

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function fileLanguage(path: string, plainText: string) {
  const extension = path.split(".").pop()?.toLowerCase();
  if (!extension || extension === path) return plainText;
  return extension === "md" ? "markdown" : extension;
}

export function EditorPane({
  modified,
  onSave,
  onUploadAttachments,
  path,
}: EditorPaneProps) {
  const { t } = useI18n();
  const [contents, setContents] = useState("");
  const [savedContents, setSavedContents] = useState("");
  const [loadState, setLoadState] = useState<LoadState>("loading");
  const [error, setError] = useState("");
  const [fileSize, setFileSize] = useState<number>();
  const [imageDimensions, setImageDimensions] = useState<{ height: number; width: number }>();
  const [imageUrl, setImageUrl] = useState<string>();
  const [saving, setSaving] = useState(false);
  const [uploading, setUploading] = useState(false);
  const attachmentInputRef = useRef<HTMLInputElement>(null);
  const imageUrlRef = useRef<string | undefined>(undefined);
  const loadTokenRef = useRef(0);
  const dirty = contents !== savedContents;
  const dirtyRef = useRef(dirty);
  dirtyRef.current = dirty;
  const readOnly = path.startsWith(".graft/");

  const clearImageUrl = useCallback(() => {
    if (imageUrlRef.current) URL.revokeObjectURL(imageUrlRef.current);
    imageUrlRef.current = undefined;
    setImageUrl(undefined);
  }, []);

  const load = useCallback(async () => {
    const loadToken = ++loadTokenRef.current;
    setLoadState("loading");
    setError("");
    setFileSize(undefined);
    setImageDimensions(undefined);
    clearImageUrl();
    try {
      if (imageExtension(path)) {
        const file = await readOpfsFile(path);
        if (loadToken !== loadTokenRef.current) return;
        setContents("");
        setSavedContents("");
        setFileSize(file.size);
        if (file.size > MAX_IMAGE_PREVIEW_BYTES) {
          setLoadState("too_large");
          return;
        }
        const nextImageUrl = URL.createObjectURL(file);
        imageUrlRef.current = nextImageUrl;
        setImageUrl(nextImageUrl);
        setLoadState("image");
        return;
      }
      const preview = await readOpfsPreview(path);
      if (loadToken !== loadTokenRef.current) return;
      setFileSize(preview.size);
      if (preview.kind !== "text") {
        setContents("");
        setSavedContents("");
        setLoadState(preview.kind);
        return;
      }
      setContents(preview.text ?? "");
      setSavedContents(preview.text ?? "");
      setLoadState("ready");
    } catch (nextError) {
      if (loadToken !== loadTokenRef.current) return;
      setError(nextError instanceof Error ? nextError.message : String(nextError));
      setLoadState("error");
    }
  }, [clearImageUrl, path]);

  useEffect(() => {
    if (!dirtyRef.current) void load();
  }, [load, modified]);

  useEffect(
    () => () => {
      loadTokenRef.current += 1;
      if (imageUrlRef.current) URL.revokeObjectURL(imageUrlRef.current);
      imageUrlRef.current = undefined;
    },
    [],
  );

  const save = useCallback(async () => {
    if (!dirty || readOnly || loadState !== "ready") return;
    setSaving(true);
    setError("");
    try {
      await onSave(path, contents);
      setSavedContents(contents);
    } catch (nextError) {
      setError(nextError instanceof Error ? nextError.message : String(nextError));
    } finally {
      setSaving(false);
    }
  }, [contents, dirty, loadState, onSave, path, readOnly]);

  const lineCount = useMemo(() => contents.split("\n").length, [contents]);

  const uploadAttachments = useCallback(
    async (files: File[]) => {
      if (files.length === 0 || uploading) return;
      setUploading(true);
      setError("");
      try {
        await onUploadAttachments(files);
      } catch (nextError) {
        setError(nextError instanceof Error ? nextError.message : String(nextError));
      } finally {
        setUploading(false);
      }
    },
    [onUploadAttachments, uploading],
  );

  return (
    <section className="editor-surface" aria-label={t("editor.label", { path })}>
      <header className="surface-tabbar">
        <div className="surface-file-tab">
          <span className="file-glyph" aria-hidden="true">
            {imageExtension(path) ? "▧" : path.endsWith(".md") ? "M" : "·"}
          </span>
          <strong>{path}</strong>
          {dirty && <i aria-label={t("editor.unsaved")} />}
        </div>
        <div className="surface-actions">
          {readOnly && <span>{t("editor.readOnly")}</span>}
          {!readOnly && loadState === "ready" && (
            <>
              <span
                className="attachment-destination"
                title={t("editor.attachmentHint", { path: "/attachments/" })}
              >
                <span>{t("editor.attachments")}</span>
                <code>/attachments/</code>
              </span>
              <input
                aria-label={t("editor.chooseAttachments")}
                className="attachment-input"
                multiple
                onChange={(event) => {
                  const files = Array.from(event.target.files ?? []);
                  event.target.value = "";
                  void uploadAttachments(files);
                }}
                ref={attachmentInputRef}
                type="file"
              />
              <button
                className="attachment-upload"
                disabled={uploading}
                onClick={() => attachmentInputRef.current?.click()}
                title={t("editor.attachmentHint", { path: "/attachments/" })}
                type="button"
              >
                <span aria-hidden="true">+</span>
                {uploading ? t("editor.uploading") : t("editor.upload")}
              </button>
              <button disabled={!dirty || saving} onClick={() => void save()} type="button">
                {saving ? t("editor.saving") : t("editor.save")}
                <kbd>⌘S</kbd>
              </button>
            </>
          )}
        </div>
      </header>

      <div className="editor-content">
        {loadState === "ready" ? (
          <textarea
            aria-label={t("editor.edit", { path })}
            onChange={(event) => setContents(event.target.value)}
            onKeyDown={(event) => {
              if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "s") {
                event.preventDefault();
                void save();
              }
            }}
            readOnly={readOnly}
            spellCheck={false}
            value={contents}
          />
        ) : loadState === "image" && imageUrl ? (
          <div className="image-preview" aria-label={t("editor.imagePreview", { path })}>
            <img
              alt={t("editor.imageAlt", { path })}
              decoding="async"
              draggable={false}
              onError={() => {
                clearImageUrl();
                setError(t("editor.imageDecodeError"));
                setLoadState("error");
              }}
              onLoad={(event) => {
                setImageDimensions({
                  height: event.currentTarget.naturalHeight,
                  width: event.currentTarget.naturalWidth,
                });
              }}
              src={imageUrl}
            />
          </div>
        ) : (
          <div className="surface-message">
            <span aria-hidden="true">{loadState === "loading" ? "…" : "◇"}</span>
            <strong>
              {loadState === "loading" && t("editor.opening")}
              {loadState === "binary" && t("editor.binary")}
              {loadState === "too_large" && t("editor.tooLarge")}
              {loadState === "error" && t("editor.openError")}
            </strong>
            <p>
              {error ||
                (loadState === "binary"
                  ? t("editor.binaryHelp")
                  : loadState === "too_large"
                    ? t("editor.tooLargeHelp")
                    : t("editor.emptyHelp"))}
            </p>
          </div>
        )}
      </div>

      <footer className="surface-statusbar">
        <span>{fileLanguage(path, t("editor.plainText"))}</span>
        {loadState === "ready" && (
          <span>{t(lineCount === 1 ? "editor.oneLine" : "editor.lines", { count: lineCount })}</span>
        )}
        {loadState === "image" && imageDimensions && (
          <span>{imageDimensions.width} × {imageDimensions.height} PX</span>
        )}
        {loadState === "image" && fileSize !== undefined && <span>{formatBytes(fileSize)}</span>}
        {error && <span className="surface-error">{error}</span>}
      </footer>
    </section>
  );
}
