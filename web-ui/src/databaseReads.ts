/** Serialize database inspections across detail views and all explorer instances. */
export function createReadQueue() {
  let tail: Promise<unknown> = Promise.resolve();
  return function enqueue<T>(read: () => Promise<T>, signal?: AbortSignal): Promise<T> {
    const result = tail.then(() => {
      signal?.throwIfAborted();
      return read();
    });
    tail = result.catch(() => {});
    return result;
  };
}
