import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [
    cloudflareTest({
      wrangler: { configPath: "./wrangler.jsonc" },
      miniflare: {
        bindings: {
          GRAFT_REMOTE_TOKEN: "test-token",
        },
      },
    }),
  ],
  test: {
    coverage: { enabled: false },
  },
});
