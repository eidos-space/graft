/** Canonical directory IDs, parents first, including explicit database containers. */
export function treeDirectories(paths: readonly string[]): string[] {
  const directories = new Set<string>();
  for (const path of paths) {
    for (let index = path.indexOf("/"); index >= 0; index = path.indexOf("/", index + 1))
      directories.add(path.slice(0, index + 1));
  }
  return [...directories].sort(
    (left, right) => left.length - right.length || left.localeCompare(right),
  );
}

/** Bulk disclosure must never turn lazy database nodes into database reads. */
export function expandableDirectories(
  directories: readonly string[],
  databaseContainers: ReadonlySet<string>,
): string[] {
  return directories.filter((path) => !databaseContainers.has(path));
}
