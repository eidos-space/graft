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
    label: "Start Here",
    translations: { "zh-CN": "开始使用" },
    items: [
      {
        label: "What Is Graft",
        translations: { "zh-CN": "Graft 是什么" },
        slug: "docs/overview/what-is-graft",
      },
      {
        label: "Choose An Integration",
        translations: { "zh-CN": "选择接入方式" },
        slug: "docs/getting-started",
      },
      {
        label: "Installation",
        translations: { "zh-CN": "安装" },
        slug: "docs/overview/installation",
      },
      {
        label: "Try The Playground",
        translations: { "zh-CN": "体验 Playground" },
        slug: "docs/quickstart/playground",
      },
    ],
  },
  {
    label: "Graft CLI",
    items: [
      {
        label: "Quickstart",
        translations: { "zh-CN": "快速开始" },
        slug: "docs/quickstart/cli",
      },
      {
        label: "Track App State",
        translations: { "zh-CN": "跟踪应用状态" },
        slug: "docs/guides/track-databases-and-files",
      },
      {
        label: "Save And Restore History",
        translations: { "zh-CN": "历史与恢复" },
        slug: "docs/guides/history-and-restore",
      },
      {
        label: "Review Changes",
        translations: { "zh-CN": "查看变更" },
        slug: "docs/guides/diff-rows-and-files",
      },
      {
        label: "Branches And Merge",
        translations: { "zh-CN": "分支与合并" },
        slug: "docs/guides/merge-conflicts",
      },
      {
        label: "External Payloads",
        translations: { "zh-CN": "外部载荷" },
        slug: "docs/guides/external-payloads",
      },
      {
        label: "Export SQLite Files",
        translations: { "zh-CN": "导出 SQLite 文件" },
        slug: "docs/guides/export-sqlite",
      },
      {
        label: "Build A UI With JSON",
        translations: { "zh-CN": "使用 JSON 构建界面" },
        slug: "docs/guides/json-ui",
      },
      {
        label: "CLI Reference",
        translations: { "zh-CN": "CLI 参考" },
        slug: "docs/reference/cli",
      },
      {
        label: "JSON Output",
        translations: { "zh-CN": "JSON 输出" },
        slug: "docs/reference/json-output",
      },
    ],
  },
  {
    label: "Node.js SDK",
    translations: { "zh-CN": "Node.js SDK" },
    items: [
      {
        label: "SDK Overview",
        translations: { "zh-CN": "SDK 概览" },
        slug: "docs/sdk",
      },
      {
        label: "SDK Quickstart",
        translations: { "zh-CN": "SDK 快速开始" },
        slug: "docs/sdk/quickstart",
      },
      {
        label: "Sessions And Worktree Safety",
        translations: { "zh-CN": "会话与工作区安全" },
        slug: "docs/sdk/session-lifecycle",
      },
    ],
  },
  {
    label: "Remote Service",
    translations: { "zh-CN": "远端服务" },
    items: [
      {
        label: "Remote Overview",
        translations: { "zh-CN": "远端概览" },
        slug: "docs/remotes",
      },
      {
        label: "Sync With Remotes",
        translations: { "zh-CN": "使用远端同步" },
        slug: "docs/guides/sync-remotes",
      },
      {
        label: "Build An HTTP Remote",
        translations: { "zh-CN": "构建 HTTP 远端" },
        slug: "docs/guides/http-remote",
      },
      {
        label: "Remote URIs",
        translations: { "zh-CN": "远端 URI" },
        slug: "docs/reference/remote-uris",
      },
      {
        label: "Remote Service Protocol",
        translations: { "zh-CN": "远端服务协议" },
        slug: "docs/reference/remote-protocol",
      },
    ],
  },
  {
    label: "Architecture And Specifications",
    translations: { "zh-CN": "架构与规范" },
    collapsed: true,
    items: [
      {
        label: "Architecture Guide",
        translations: { "zh-CN": "架构导读" },
        slug: "docs/graft-book",
      },
      {
        label: "Repository And Objects",
        translations: { "zh-CN": "仓库、对象与引用" },
        slug: "docs/graft-book/repository-and-objects",
      },
      {
        label: "SQLite Snapshots",
        translations: { "zh-CN": "SQLite 快照" },
        slug: "docs/graft-book/sqlite-snapshots",
      },
      {
        label: "Stage And Commit",
        translations: { "zh-CN": "暂存与提交" },
        slug: "docs/graft-book/stage-and-commit",
      },
      {
        label: "Checkout And Restore",
        translations: { "zh-CN": "检出与恢复" },
        slug: "docs/graft-book/checkout-and-restore",
      },
      {
        label: "Diff And Merge",
        translations: { "zh-CN": "差异与合并" },
        slug: "docs/graft-book/diff-and-merge",
      },
      {
        label: "Remotes And Recovery",
        translations: { "zh-CN": "远端与恢复" },
        slug: "docs/graft-book/remotes-and-recovery",
      },
      {
        label: "Hands-On Lab",
        translations: { "zh-CN": "架构实验" },
        slug: "docs/graft-book/hands-on-lab",
      },
      {
        label: "Specifications",
        translations: { "zh-CN": "规范" },
        slug: "docs/specifications",
      },
    ],
  },
  {
    label: "Reference",
    translations: { "zh-CN": "参考" },
    collapsed: true,
    items: [
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
        label: "Glossary",
        translations: { "zh-CN": "术语表" },
        slug: "docs/reference/glossary",
      },
      {
        label: "Troubleshooting",
        translations: { "zh-CN": "故障排查" },
        slug: "docs/reference/troubleshooting",
      },
      {
        label: "Project Status",
        translations: { "zh-CN": "项目状态" },
        slug: "docs/overview/status",
      },
    ],
  },
] satisfies SidebarSection[];
