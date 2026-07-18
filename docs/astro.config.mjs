// @ts-check
import { execSync } from "node:child_process";
import { readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import starlightDocSearch from "@astrojs/starlight-docsearch";
import starlightThemeFlexoki from "starlight-theme-flexoki";
import sitemap from "@astrojs/sitemap";
import llmifyPlugin from "./src/plugins/llmify";
import { sidebar } from "./src/config/sidebar";
import { DOC_REDIRECTS } from "./src/config/redirects";

// find the current branch name
export function currentBranch() {
  const branch =
    process.env.GITHUB_HEAD_REF || // PR source branch in GitHub Actions
    process.env.GITHUB_REF_NAME || // push/tag ref in GitHub Actions
    process.env.CF_PAGES_BRANCH; // branch name in CloudFlare pages build

  if (branch) {
    return branch;
  }

  // fallback to checking git
  return execSync("git rev-parse --abbrev-ref HEAD", {
    encoding: "utf8",
  }).trim();
}

/** @returns {import("astro").AstroIntegration} */
function removeFavicon() {
  return {
    name: "remove-favicon",
    hooks: {
      "astro:build:done": ({ dir }) => {
        const root = fileURLToPath(dir);
        for (const file of walkHtmlFiles(root)) {
          const html = readFileSync(file, "utf8");
          const next = html.replace(
            /<link(?=[^>]*\brel="shortcut icon")(?=[^>]*\bhref="\/favicon\.svg")[^>]*>/g,
            "",
          );
          if (next !== html) {
            writeFileSync(file, next);
          }
        }
      },
    },
  };
}

/**
 * @param {string} dir
 * @returns {Generator<string, void, unknown>}
 */
function* walkHtmlFiles(dir) {
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) {
      yield* walkHtmlFiles(path);
    } else if (path.endsWith(".html")) {
      yield path;
    }
  }
}

// https://astro.build/config
export default defineConfig({
  site: "https://graft.eidos.space/",
  redirects: DOC_REDIRECTS,
  integrations: [
    starlight({
      plugins: [
        starlightThemeFlexoki({
          accentColor: "green",
        }),
        starlightDocSearch({
          clientOptionsModule: "./src/config/docsearch.ts",
        }),
      ],
      title: "Graft",
      pagination: false,
      head: [
        {
          tag: "script",
          attrs: {
            src: "https://cdn.usefathom.com/script.js",
            "data-site": "MEZQWTLT",
            defer: true,
          },
        },
        {
          tag: "link",
          attrs: {
            rel: "sitemap",
            href: "/sitemap-index.xml",
          },
        },
        {
          tag: "link",
          attrs: {
            rel: "preconnect",
            href: "https://gs869rqcpn-dsn.algolia.net",
            crossorigin: true,
          },
        },
      ],
      lastUpdated: true,
      locales: {
        root: {
          label: "English",
          lang: "en",
        },
        zh: {
          label: "简体中文",
          lang: "zh-CN",
        },
      },
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/eidos-space/graft",
        },
      ],
      customCss: ["./src/styles/global.css"],
      editLink: {
        baseUrl: `https://github.com/eidos-space/graft/blob/${currentBranch()}/docs/`,
      },
      sidebar,
    }),
    sitemap(),
    llmifyPlugin(),
    removeFavicon(),
  ],
});
