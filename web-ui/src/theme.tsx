import {
  createContext,
  useContext,
  useEffect,
  useInsertionEffect,
  useState,
  type ReactNode,
} from "react";
import { readPreference, writePreference } from "./preferences";

type Theme = "dark" | "light";

const ThemeContext = createContext<Theme>("dark");

function readTheme(): Theme {
  return readPreference("theme") ?? "dark";
}

export function ThemeProvider({ children }: { children: ReactNode }) {
  const [theme, setTheme] = useState(readTheme);

  // Canvas consumers read semantic colors in layout effects, before painting.
  useInsertionEffect(() => {
    document.documentElement.dataset.theme = theme;
    writePreference("theme", theme);
  }, [theme]);

  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (
        event.isComposing ||
        !(event.metaKey || event.ctrlKey) ||
        !event.shiftKey ||
        event.altKey ||
        (event.code !== "KeyL" && event.key.toLowerCase() !== "l")
      )
        return;
      event.preventDefault();
      event.stopPropagation();
      if (!event.repeat) setTheme((current) => (current === "dark" ? "light" : "dark"));
    };
    window.addEventListener("keydown", handleKeyDown, true);
    return () => window.removeEventListener("keydown", handleKeyDown, true);
  }, []);

  return <ThemeContext.Provider value={theme}>{children}</ThemeContext.Provider>;
}

export function useTheme() {
  return useContext(ThemeContext);
}
