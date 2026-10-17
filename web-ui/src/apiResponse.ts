/** Preserve useful service errors even if a restarted server returns HTML. */
export async function readApiResponse<T>(response: Response): Promise<T> {
  if (response.status === 401 || response.status === 403)
    throw new Error(
      "This tab is not authorized for the local session. Open the complete URL printed by graft . in the terminal, including #token=….",
    );
  let data: unknown;
  try {
    data = await response.json();
  } catch {
    throw new Error(
      response.ok
        ? "The local Graft service returned an invalid response. Retry or open its latest terminal URL."
        : `The local Graft service could not complete this request (HTTP ${response.status}). Retry or open its latest terminal URL.`,
    );
  }
  if (!response.ok) {
    const message =
      typeof data === "object" && data !== null && "error" in data
        ? String(data.error)
        : `Request failed (HTTP ${response.status}). Please retry.`;
    throw new Error(message);
  }
  return data as T;
}
