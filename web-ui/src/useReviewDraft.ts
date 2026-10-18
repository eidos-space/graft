import { useCallback, useState, type SetStateAction } from "react";
import { readReviewDraft, writeReviewDraft } from "./reviewDrafts";

/** Synchronous persistence also covers switching files immediately after choosing. */
export function useReviewDraft<T>(key: string, initial: T) {
  const [value, setValue] = useState(() => readReviewDraft(key, initial));
  const update = useCallback(
    (action: SetStateAction<T>) => {
      setValue((previous) => {
        const next = action instanceof Function ? action(previous) : action;
        writeReviewDraft(key, next);
        return next;
      });
    },
    [key],
  );
  return [value, update] as const;
}
