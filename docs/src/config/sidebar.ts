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
        label: "Node.js And Electron SDK",
        translations: { "zh-CN": "Node.js 与 Electron SDK" },
        slug: "docs/guides/node-electron-sdk",
      },
      {
        label: "Export SQLite Files",
        translations: { "zh-CN": "导出 SQLite 文件" },
        slug: "docs/guides/export-sqlite",
      },
    ],
  },
  {
    label: "Inside Graft",
    translations: { "zh-CN": "原理：数据如何流动" },
    items: [
      {
        label: "Reading Guide",
        translations: { "zh-CN": "00 阅读指南与心智模型" },
        slug: "docs/graft-book",
      },
      {
        label: "Repository And Objects",
        translations: { "zh-CN": "01 仓库、对象与引用" },
        slug: "docs/graft-book/repository-and-objects",
      },
      {
        label: "SQLite Snapshots",
        translations: { "zh-CN": "02 SQLite 如何变成快照" },
        slug: "docs/graft-book/sqlite-snapshots",
      },
      {
        label: "Stage And Commit",
        translations: { "zh-CN": "03 从 add 到 commit" },
        slug: "docs/graft-book/stage-and-commit",
      },
      {
        label: "Checkout And Restore",
        translations: { "zh-CN": "04 从快照回到工作区" },
        slug: "docs/graft-book/checkout-and-restore",
      },
      {
        label: "Diff And Merge",
        translations: { "zh-CN": "05 Diff、Merge 与冲突" },
        slug: "docs/graft-book/diff-and-merge",
      },
      {
        label: "Remotes And Recovery",
        translations: { "zh-CN": "06 远端、失败与恢复" },
        slug: "docs/graft-book/remotes-and-recovery",
      },
      {
        label: "Hands-On Lab",
        translations: { "zh-CN": "07 动手观察每一次变化" },
        slug: "docs/graft-book/hands-on-lab",
      },
    ],
  },
  {
    label: "Reference",
    translations: { "zh-CN": "查阅手册" },
    collapsed: true,
    items: [
      { label: "CLI", slug: "docs/reference/cli" },
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
        label: "Remote Service Protocol",
        translations: { "zh-CN": "Remote Service 协议" },
        slug: "docs/reference/remote-protocol",
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
] satisfies SidebarSection[];
