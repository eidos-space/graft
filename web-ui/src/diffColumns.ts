import type { RowChange } from "./types";

export function diffIdentityColumns(
  primaryKeys: string[] | undefined,
  rowidAlias: string | undefined,
  changes: RowChange[],
) {
  const keys = new Set(primaryKeys);
  if (rowidAlias) keys.add(rowidAlias);
  for (const change of changes) for (const key of Object.keys(change.key ?? {})) keys.add(key);
  return { primaryKeys: keys, showRowid: keys.size === 0 };
}
