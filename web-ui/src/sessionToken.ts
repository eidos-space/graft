interface SessionAccess {
  hash: string;
  read: () => string | null;
  save: (token: string) => void;
  clearFragment: () => void;
}

/** A same-document URL change can supply a new host session after a restart. */
export function readSessionToken(fallback: string, access: SessionAccess): string {
  const fragment = new URLSearchParams(access.hash.slice(1));
  const provided = fragment.get("token");
  if (provided) {
    try {
      access.save(provided);
      access.clearFragment();
    } catch {
      // Keep the URL token so reloading works if storage is blocked or full.
    }
    return provided;
  }
  try {
    return access.read() ?? fallback;
  } catch {
    return fallback;
  }
}
