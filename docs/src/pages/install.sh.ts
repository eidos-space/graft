import { readFile } from "node:fs/promises";

import type { APIRoute } from "astro";

const installerUrl = new URL("../../../install.sh", import.meta.url);

export const prerender = true;

export const GET: APIRoute = async () =>
  new Response(await readFile(installerUrl), {
    headers: {
      "Content-Type": "text/x-shellscript; charset=utf-8",
    },
  });
