export type PreviewKind = "image" | "audio" | "video" | "pdf";

/** Only binary media bypass the text diff/editor. */
export function previewKind(path: string): PreviewKind | undefined {
  const extension = path.split(".").pop()?.toLowerCase() ?? "";
  if (
    ["png", "apng", "jpg", "jpeg", "jfif", "gif", "webp", "avif", "ico", "bmp"].includes(extension)
  )
    return "image";
  if (["mp3", "m4a", "aac", "wav", "ogg", "oga", "opus", "flac", "weba"].includes(extension))
    return "audio";
  if (["mp4", "m4v", "webm", "ogv", "mov"].includes(extension)) return "video";
  if (extension === "pdf") return "pdf";
}
