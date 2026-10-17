import { useCallback, useEffect, useRef, useState } from "react";
import type { ApiInput } from "./types";
import { createReadQueue } from "./databaseReads";
import { readApiResponse } from "./apiResponse";
import { readSessionToken } from "./sessionToken";

const databaseRead = createReadQueue();
const databaseEndpoints = new Set([
  "database",
  "database-cell",
  "database-tables",
  "sqlite-diff",
  "conflict",
]);

let token = "";
function sessionToken() {
  token = readSessionToken(token, {
    hash: location.hash,
    read: () => sessionStorage.getItem("graft-session"),
    save: (value) => sessionStorage.setItem("graft-session", value),
    clearFragment: () => history.replaceState(null, "", location.pathname + location.search),
  });
  return token;
}
sessionToken();

export function previewUrl(path: string): string {
  const encoded = Array.from(new TextEncoder().encode(path), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
  return `/api/preview/${encoded}?token=${encodeURIComponent(sessionToken())}`;
}

export async function api<T>(
  endpoint: string,
  input: ApiInput = {},
  signal?: AbortSignal,
): Promise<T> {
  const read = async () => {
    signal?.throwIfAborted();
    let response: Response;
    try {
      response = await fetch(`/api/${endpoint}`, {
        method: "POST",
        headers: { "Content-Type": "application/json", Authorization: `Bearer ${sessionToken()}` },
        body: JSON.stringify(input),
        signal,
      });
    } catch (error) {
      if (signal?.aborted) throw error;
      throw new Error(
        "The local Graft service is unavailable. Restart graft . in the repository and open the new URL printed in the terminal.",
      );
    }
    return readApiResponse<T>(response);
  };
  return databaseEndpoints.has(endpoint) ? databaseRead(read, signal) : read();
}

// Keep the last successful value while refreshing, and cancel superseded reads.
export function useResource<T>(
  endpoint: string,
  input: ApiInput,
  live = false,
  paused = false,
  enabled = true,
  refreshKey: string | number = 0,
) {
  const key = JSON.stringify([endpoint, input, refreshKey]);
  const identity = JSON.stringify([endpoint, input]);
  const [state, setState] = useState<{ key: string; identity: string; data?: T; error?: string }>({
    key,
    identity,
  });
  const pausedRef = useRef(paused);
  pausedRef.current = paused;
  const [attempt, setAttempt] = useState(0);
  const retry = useCallback(() => setAttempt((value) => value + 1), []);
  useEffect(() => {
    if (!enabled || paused) return;
    let disposed = false;
    let pending = false;
    let version: unknown;
    let serialized: string | undefined;
    const controller = new AbortController();
    const refresh = async () => {
      if (pending || pausedRef.current || (live && document.hidden && serialized !== undefined))
        return;
      pending = true;
      try {
        const data = await api<T>(
          endpoint,
          endpoint === "database" && version !== undefined
            ? { ...input, expected: JSON.stringify(version) }
            : input,
          controller.signal,
        );
        if (data && typeof data === "object" && "unchanged" in data && data.unchanged === true) {
          if (!disposed) setState((previous) => ({ ...previous, error: undefined }));
          return;
        }
        if (disposed || pausedRef.current) return;
        if (data && typeof data === "object" && "version" in data) version = data.version;
        const nextSerialized = JSON.stringify(data);
        const same = serialized === nextSerialized;
        serialized = nextSerialized;
        if (!disposed && !pausedRef.current)
          setState((previous) => {
            if (previous.key === key && !previous.error && same) return previous;
            return { key, identity, data };
          });
      } catch (error) {
        if (!disposed)
          setState((previous) => ({
            key,
            identity,
            data: previous.identity === identity ? previous.data : undefined,
            error: error instanceof Error ? error.message : "Could not load content",
          }));
      } finally {
        pending = false;
      }
    };
    void refresh();
    const timer = live ? window.setInterval(() => void refresh(), 3000) : undefined;
    const onVisible = () => {
      if (!document.hidden) void refresh();
    };
    if (live) document.addEventListener("visibilitychange", onVisible);
    return () => {
      disposed = true;
      controller.abort();
      clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisible);
    };
    // The serialized input is the resource identity.
  }, [key, live, enabled, paused, attempt]);
  const result =
    state.key === key
      ? state
      : { key, data: state.identity === identity ? state.data : undefined, error: undefined };
  return { ...result, retry };
}
