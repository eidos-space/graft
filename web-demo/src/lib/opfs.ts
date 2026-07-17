import type { OpfsEntry } from "../types";

type IterableDirectory = FileSystemDirectoryHandle & {
  entries(): AsyncIterableIterator<[string, FileSystemHandle]>;
};

export interface OpfsListEntry {
  kind: "directory" | "file";
  name: string;
  path: string;
  size?: number;
}

export interface OpfsPreview {
  kind: "binary" | "text" | "too_large";
  size: number;
  text?: string;
}

async function rootDirectory() {
  return navigator.storage.getDirectory();
}

export function resolveOpfsPath(path: string, cwd = "/") {
  const parts = path.startsWith("/") ? [] : cwd.split("/").filter(Boolean);
  for (const part of path.split("/")) {
    if (!part || part === ".") continue;
    if (part === "..") parts.pop();
    else parts.push(part);
  }
  return `/${parts.join("/")}`;
}

function pathParts(path: string, cwd = "/") {
  return resolveOpfsPath(path, cwd).split("/").filter(Boolean);
}

async function directoryHandle(path: string, cwd = "/", create = false) {
  let directory = await rootDirectory();
  for (const part of pathParts(path, cwd)) {
    directory = await directory.getDirectoryHandle(part, { create });
  }
  return directory;
}

async function parentAndName(path: string, cwd = "/", createParents = false) {
  const parts = pathParts(path, cwd);
  const name = parts.pop();
  if (!name) throw new Error("A path below / is required");
  const parent = await directoryHandle(`/${parts.join("/")}`, "/", createParents);
  return { name, parent };
}

async function fileHandle(
  path: string,
  cwd = "/",
  create = false,
  createParents = false,
) {
  const { name, parent } = await parentAndName(path, cwd, createParents);
  return parent.getFileHandle(name, { create });
}

export async function scanOpfs(): Promise<OpfsEntry[]> {
  const root = await rootDirectory();
  const entries: OpfsEntry[] = [];

  async function visit(directory: FileSystemDirectoryHandle, prefix = "") {
    for await (const [name, handle] of (directory as IterableDirectory).entries()) {
      const path = `${prefix}${name}`;
      if (handle.kind === "directory") {
        entries.push({ kind: "directory", path: `${path}/` });
        await visit(handle as FileSystemDirectoryHandle, `${path}/`);
      } else {
        const file = await (handle as FileSystemFileHandle).getFile();
        entries.push({
          kind: "file",
          modified: file.lastModified,
          path,
          size: file.size,
        });
      }
    }
  }

  await visit(root);
  return entries.sort((left, right) => left.path.localeCompare(right.path));
}

export async function clearOpfs() {
  const root = await rootDirectory();
  const names: string[] = [];
  for await (const [name] of (root as IterableDirectory).entries()) names.push(name);
  for (const name of names) await root.removeEntry(name, { recursive: true });
}

export async function statOpfsPath(path: string, cwd = "/") {
  const absolute = resolveOpfsPath(path, cwd);
  if (absolute === "/") return { kind: "directory" as const, path: absolute };
  const { name, parent } = await parentAndName(absolute);
  try {
    const directory = await parent.getDirectoryHandle(name);
    return { handle: directory, kind: "directory" as const, path: absolute };
  } catch {
    const handle = await parent.getFileHandle(name);
    const file = await handle.getFile();
    return { handle, kind: "file" as const, path: absolute, size: file.size };
  }
}

export async function listOpfsDirectory(path = ".", cwd = "/") {
  const absolute = resolveOpfsPath(path, cwd);
  const directory = await directoryHandle(absolute);
  const entries: OpfsListEntry[] = [];
  for await (const [name, handle] of (directory as IterableDirectory).entries()) {
    if (handle.kind === "directory") {
      entries.push({ kind: "directory", name, path: resolveOpfsPath(name, absolute) });
    } else {
      const file = await (handle as FileSystemFileHandle).getFile();
      entries.push({
        kind: "file",
        name,
        path: resolveOpfsPath(name, absolute),
        size: file.size,
      });
    }
  }
  return entries.sort((left, right) => {
    if (left.kind !== right.kind) return left.kind === "directory" ? -1 : 1;
    return left.name.localeCompare(right.name);
  });
}

export async function readOpfsPreview(
  path: string,
  cwd = "/",
  maxBytes = 512 * 1024,
): Promise<OpfsPreview> {
  const handle = await fileHandle(path, cwd);
  const file = await handle.getFile();
  if (file.size > maxBytes) return { kind: "too_large", size: file.size };
  const bytes = await file.arrayBuffer();
  try {
    const text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    if (text.includes("\0")) return { kind: "binary", size: file.size };
    return { kind: "text", size: file.size, text };
  } catch {
    return { kind: "binary", size: file.size };
  }
}

export async function readOpfsFile(path: string, cwd = "/") {
  const handle = await fileHandle(path, cwd);
  return handle.getFile();
}

export async function readOpfsText(path: string, cwd = "/") {
  const preview = await readOpfsPreview(path, cwd);
  if (preview.kind === "too_large") throw new Error("File is larger than the preview limit");
  if (preview.kind === "binary") throw new Error("File is not UTF-8 text");
  return preview.text ?? "";
}

export async function writeOpfsText(path: string, contents: string, cwd = "/") {
  const handle = await fileHandle(path, cwd, true);
  const writable = await handle.createWritable();
  await writable.write(contents);
  await writable.close();
}

export async function writeOpfsBlob(
  path: string,
  contents: Blob,
  cwd = "/",
  createParents = false,
) {
  const handle = await fileHandle(path, cwd, true, createParents);
  const writable = await handle.createWritable();
  await writable.write(contents);
  await writable.close();
}

const ATTACHMENTS_DIRECTORY = "/attachments";

function safeAttachmentName(name: string) {
  const sanitized = name
    .normalize("NFKC")
    .replace(/[\\/\u0000-\u001f\u007f]/g, "-")
    .trim();
  return sanitized && !/^\.+$/.test(sanitized) ? sanitized.slice(0, 180) : "attachment";
}

function numberedAttachmentName(name: string, number: number) {
  const dot = name.lastIndexOf(".");
  const hasExtension = dot > 0 && dot < name.length - 1;
  const stem = hasExtension ? name.slice(0, dot) : name;
  const extension = hasExtension ? name.slice(dot) : "";
  return `${stem} (${number})${extension}`;
}

export async function writeOpfsAttachments(files: readonly File[]) {
  const directory = await directoryHandle(ATTACHMENTS_DIRECTORY, "/", true);
  const reservedNames = new Set<string>();
  for await (const [name] of (directory as IterableDirectory).entries()) {
    reservedNames.add(name);
  }

  const paths: string[] = [];
  for (const file of files) {
    const originalName = safeAttachmentName(file.name);
    let name = originalName;
    let number = 2;
    while (reservedNames.has(name)) {
      name = numberedAttachmentName(originalName, number);
      number += 1;
    }

    const handle = await directory.getFileHandle(name, { create: true });
    const writable = await handle.createWritable();
    await writable.write(file);
    await writable.close();
    reservedNames.add(name);
    paths.push(`attachments/${name}`);
  }
  return paths;
}

export async function createOpfsDirectory(path: string, cwd = "/", recursive = false) {
  if (recursive) {
    await directoryHandle(path, cwd, true);
    return;
  }
  const { name, parent } = await parentAndName(path, cwd);
  await parent.getDirectoryHandle(name, { create: true });
}

export async function touchOpfsFile(path: string, cwd = "/") {
  await fileHandle(path, cwd, true);
}

export async function removeOpfsPath(path: string, cwd = "/", recursive = false) {
  const absolute = resolveOpfsPath(path, cwd);
  if (absolute === "/") throw new Error("Refusing to remove OPFS root");
  const { name, parent } = await parentAndName(absolute);
  await parent.removeEntry(name, { recursive });
}
