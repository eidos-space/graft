const memory = new Map<string, unknown>();
const prefix = "graft-review-draft:";

export function readReviewDraft<T>(key: string, fallback: T): T {
  try {
    const raw = sessionStorage.getItem(prefix + key);
    if (raw) return JSON.parse(raw) as T;
  } catch {
    // A private browser or a full storage quota still supports navigation drafts.
  }
  return (memory.get(key) as T | undefined) ?? fallback;
}

export function writeReviewDraft<T>(key: string, value: T): void {
  memory.set(key, value);
  try {
    sessionStorage.setItem(prefix + key, JSON.stringify(value));
  } catch {
    // Keep the in-memory draft when session storage is unavailable.
  }
}

export function clearReviewDraft(key: string): void {
  memory.delete(key);
  try {
    sessionStorage.removeItem(prefix + key);
  } catch {
    // The in-memory copy was already removed.
  }
}
