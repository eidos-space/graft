import assert from "node:assert/strict";
import { test } from "node:test";
import { readApiResponse } from "../src/apiResponse.ts";

test("authorization recovery survives non-JSON service errors", async () => {
  await assert.rejects(readApiResponse(new Response("Forbidden", { status: 403 })), /#token=/);
});
test("malformed responses provide a recovery instruction instead of a JSON parser error", async () => {
  await assert.rejects(
    readApiResponse(new Response("<html>Bad gateway</html>", { status: 502 })),
    /HTTP 502/,
  );
  await assert.rejects(readApiResponse(new Response("truncated")), /invalid response.*Retry/);
});
test("structured service errors retain their original explanation", async () => {
  await assert.rejects(
    readApiResponse(Response.json({ error: "The file changed externally" }, { status: 409 })),
    /file changed externally/,
  );
  assert.deepEqual(await readApiResponse(Response.json({ rows: [] })), { rows: [] });
});
