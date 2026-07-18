// Shared sidebar configuration used by both astro.config.mjs and llmify plugin.

export interface SidebarItem {
  label: string;
  translations?: Record<string, string>;
  slug?: string;
  link?: string;
  items?: SidebarItem[];
  autogenerate?: { directory: string };
}

export interface SidebarSection {
  label: string;
  translations?: Record<string, string>;
  collapsed?: boolean;
  items?: SidebarItem[];
  autogenerate?: { directory: string };
}

export const sidebar = [
  {
    label: "Overview",
    translations: { "zh-CN": "概览" },
    items: [
      {
        label: "What Is Graft",
        translations: { "zh-CN": "Graft 是什么" },
        slug: "docs/overview/what-is-graft",
      },
      {
        label: "Project Status",
        translations: { "zh-CN": "项目状态" },
        slug: "docs/overview/status",
      },
      {
        label: "Installation",
        translations: { "zh-CN": "安装" },
        slug: "docs/overview/installation",
      },
    ],
  },
  {
    label: "Quickstart",
    translations: { "zh-CN": "快速开始" },
    items: [
      {
        label: "Playground",
        translations: { "zh-CN": "Playground 导览" },
        slug: "docs/quickstart/playground",
      },
      {
        label: "CLI Quickstart",
        translations: { "zh-CN": "CLI 快速开始" },
        slug: "docs/quickstart/cli",
      },
      {
        label: "SQLite Extension",
        translations: { "zh-CN": "SQLite 扩展" },
        slug: "docs/quickstart/sqlite-extension",
      },
      {
        label: "App-State Walkthrough",
        translations: { "zh-CN": "应用状态演练" },
        slug: "docs/quickstart/app-state-walkthrough",
      },
    ],
  },
  {
    label: "Guides",
    translations: { "zh-CN": "指南" },
    items: [
      {
        label: "Track Databases And Files",
        translations: { "zh-CN": "跟踪数据库和文件" },
        slug: "docs/guides/track-databases-and-files",
      },
      {
        label: "History And Restore",
        translations: { "zh-CN": "历史与恢复" },
        slug: "docs/guides/history-and-restore",
      },
      {
        label: "Diff Rows And Files",
        translations: { "zh-CN": "比较行与文件" },
        slug: "docs/guides/diff-rows-and-files",
      },
      {
        label: "Merge Conflicts",
        translations: { "zh-CN": "合并冲突" },
        slug: "docs/guides/merge-conflicts",
      },
      {
        label: "Sync With Remotes",
        translations: { "zh-CN": "远端同步" },
        slug: "docs/guides/sync-remotes",
      },
      {
        label: "External Payloads",
        translations: { "zh-CN": "外部载荷" },
        slug: "docs/guides/external-payloads",
      },
      {
        label: "App UI From JSON",
        translations: { "zh-CN": "用 JSON 构建应用 UI" },
        slug: "docs/guides/json-ui",
      },
      {
        label: "Connect An HTTP Remote",
        translations: { "zh-CN": "连接 HTTP 远端" },
        slug: "docs/guides/http-remote",
      },
      {
        label: "Export SQLite Files",
        translations: { "zh-CN": "导出 SQLite 文件" },
        slug: "docs/guides/export-sqlite",
      },
    ],
  },
  {
    label: "Concepts",
    translations: { "zh-CN": "概念" },
    collapsed: true,
    items: [
      {
        label: "App-State Versioning",
        translations: { "zh-CN": "应用状态版本管理" },
        slug: "docs/concepts/app-state-versioning",
      },
      {
        label: "Repository Model",
        translations: { "zh-CN": "仓库模型" },
        slug: "docs/concepts/repository-model",
      },
      {
        label: "SQLite Snapshots",
        translations: { "zh-CN": "SQLite 快照" },
        slug: "docs/concepts/sqlite-snapshots",
      },
      {
        label: "File Artifacts",
        translations: { "zh-CN": "文件制品" },
        slug: "docs/concepts/file-artifacts",
      },
      {
        label: "Row Diffs And Merge Policy",
        translations: { "zh-CN": "行级差异与合并策略" },
        slug: "docs/concepts/row-diffs-and-merge-policy",
      },
      {
        label: "Branches And Remotes",
        translations: { "zh-CN": "分支与远端" },
        slug: "docs/concepts/branches-and-remotes",
      },
    ],
  },
  {
    label: "Reference",
    translations: { "zh-CN": "参考" },
    collapsed: true,
    items: [
      { label: "CLI", slug: "docs/reference/cli" },
      {
        label: "SQLite PRAGMAs",
        translations: { "zh-CN": "SQLite PRAGMA" },
        slug: "docs/reference/pragmas",
      },
      {
        label: "JSON Output",
        translations: { "zh-CN": "JSON 输出" },
        slug: "docs/reference/json-output",
      },
      {
        label: "Configuration",
        translations: { "zh-CN": "配置" },
        slug: "docs/reference/configuration",
      },
      {
        label: "Merge Policy",
        translations: { "zh-CN": "合并策略" },
        slug: "docs/reference/merge-policy",
      },
      {
        label: "Remote URIs",
        translations: { "zh-CN": "远端 URI" },
        slug: "docs/reference/remote-uris",
      },
      {
        label: "Glossary",
        translations: { "zh-CN": "术语表" },
        slug: "docs/reference/glossary",
      },
      {
        label: "Troubleshooting",
        translations: { "zh-CN": "故障排查" },
        slug: "docs/reference/troubleshooting",
      },
    ],
  },
  {
    label: "Internals",
    translations: { "zh-CN": "实现原理" },
    collapsed: true,
    items: [
      {
        label: "Architecture",
        translations: { "zh-CN": "架构" },
        slug: "docs/internals/architecture",
      },
      {
        label: "Object Formats",
        translations: { "zh-CN": "对象格式" },
        slug: "docs/internals/object-formats",
      },
      {
        label: "Snapshot Storage And LSNs",
        translations: { "zh-CN": "快照存储与 LSN" },
        slug: "docs/internals/snapshot-storage-lsns",
      },
      {
        label: "Row Diff Engine",
        translations: { "zh-CN": "行级差异引擎" },
        slug: "docs/internals/row-diff-engine",
      },
      {
        label: "HTTP Remote Protocol",
        translations: { "zh-CN": "HTTP 远端协议" },
        slug: "docs/internals/http-remote-protocol",
      },
    ],
  },
] satisfies SidebarSection[];
