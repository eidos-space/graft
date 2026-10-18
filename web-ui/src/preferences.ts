interface Preferences {
  theme: "dark" | "light";
  compactFolders: boolean;
  layout: { side: "left" | "right"; width: number };
}
const cookieName = "graft-ui-preferences";
const storageKeys = {
  theme: "graft-theme",
  layout: "graft-workspace-layout",
  compactFolders: "graft-compact-folders",
};

export function parsePreferences(raw: string): Partial<Preferences> {
  try {
    const value: unknown = JSON.parse(raw);
    if (!value || typeof value !== "object") return {};
    const result: Partial<Preferences> = {};
    if ("compactFolders" in value && typeof value.compactFolders === "boolean")
      result.compactFolders = value.compactFolders;
    if ("theme" in value && (value.theme === "dark" || value.theme === "light"))
      result.theme = value.theme;
    if ("layout" in value && value.layout && typeof value.layout === "object") {
      const layout = value.layout;
      if (
        "side" in layout &&
        (layout.side === "left" || layout.side === "right") &&
        "width" in layout &&
        typeof layout.width === "number" &&
        Number.isFinite(layout.width)
      ) {
        result.layout = { side: layout.side, width: Math.max(240, Math.min(640, layout.width)) };
      }
    }
    return result;
  } catch {
    return {};
  }
}
function cookiePreferences(): Partial<Preferences> {
  try {
    const raw = document.cookie
      .split(";")
      .map((part) => part.trim())
      .find((part) => part.startsWith(`${cookieName}=`));
    return raw ? parsePreferences(decodeURIComponent(raw.slice(cookieName.length + 1))) : {};
  } catch {
    return {};
  }
}
export function readPreference<Key extends keyof Preferences>(
  key: Key,
): Preferences[Key] | undefined {
  const shared = cookiePreferences()[key];
  if (shared !== undefined) return shared;
  try {
    const raw = localStorage.getItem(storageKeys[key]);
    if (!raw) return undefined;
    return parsePreferences(JSON.stringify({ [key]: key === "theme" ? raw : JSON.parse(raw) }))[
      key
    ];
  } catch {
    return undefined;
  }
}
export function writePreference<Key extends keyof Preferences>(key: Key, value: Preferences[Key]) {
  // Cookies are host-scoped, so random loopback ports share display preferences.
  // Authentication remains in the existing per-tab session storage.
  const preferences = {
    theme: readPreference("theme"),
    compactFolders: readPreference("compactFolders"),
    layout: readPreference("layout"),
    [key]: value,
  };
  try {
    document.cookie = `${cookieName}=${encodeURIComponent(JSON.stringify(preferences))}; Path=/; Max-Age=31536000; SameSite=Strict`;
  } catch {
    /* Fall back to origin storage when cookies are unavailable. */
  }
  try {
    localStorage.setItem(
      storageKeys[key],
      typeof value === "string" ? value : JSON.stringify(value),
    );
  } catch {
    /* Keep the current session usable when browser storage is blocked. */
  }
}
