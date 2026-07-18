import type { DocSearchClientOptions } from "@astrojs/starlight-docsearch";

const canonicalOrigin = "https://graft.eidos.space";
const searchOrigin =
  window.location.hostname === "graft.eidos.space"
    ? canonicalOrigin
    : window.location.origin;

export default {
  appId: "GS869RQCPN",
  apiKey: "d6afaeb4da018efde82718b6bf1abda7",
  indexName: "graft",
  insights: true,
  getMissingResultsUrl({ query }) {
    return `https://github.com/eidos-space/graft/issues/new?title=${query}&labels=documentation`;
  },
  transformItems(items) {
    return items.map((item) => ({
      ...item,
      url: item.url.replace(
        /^https:\/\/(?:graft\.rs|graft\.eidos\.space)/,
        searchOrigin,
      ),
    }));
  },
} satisfies DocSearchClientOptions;
