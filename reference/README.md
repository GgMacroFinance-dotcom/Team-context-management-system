# 参考仓库

这里的目录都是 git 子模块，指向上游仓库的一个固定提交，只用来参考设计。本仓库不包含它们的代码，各自的许可证留在各自的仓库里，原样有效。

拉下来看：

```sh
git submodule update --init --depth 1 reference/notion-mcp-server   # 拉一个
git submodule update --init --depth 1                               # 全部拉（AFFiNE 很大）
```

不拉也不影响构建：本仓库的代码不依赖这里的任何东西。

## 清单

固定在 2026-11-22 各仓库默认分支的最新提交。许可证是 GitHub 识别出来的结果，用到某个仓库前以它自己的 LICENSE 文件为准。

| 目录 | 上游 | 许可证 | 看什么 |
| --- | --- | --- | --- |
| `affine` | toeverything/AFFiNE | 主体 MIT；`packages/backend`、`packages/common/native` 是 AFFiNE 企业版许可 | 工作区、文档、数据库视图的产品形态；Yjs 文档结构；以后和 AFFiNE 同步的接口 |
| `blocksuite` | toeverything/blocksuite | MPL-2.0 | 块模型、块的 schema、文档快照格式（同步时“一条对应一个块”的参考） |
| `octobase` | toeverything/OctoBase | AGPL-3.0 | 本地优先的存储和同步思路（Rust） |
| `affine-design` | toeverything/design | MPL-2.0 | 审核页的样式参考 |
| `affine-docker` | toeverything/docker | 未识别 | 自托管部署方式 |
| `affine-workers` | toeverything/affine-workers | 未识别 | 边缘节点上的辅助服务 |
| `affine-reader` | toeverything/affine-reader | 未识别 | 在 AFFiNE 之外读取 AFFiNE 文档：和 AFFiNE 同步时最直接的参考 |
| `notesuite` | toeverything/notesuite | 未识别 | 基于 BlockSuite 的轻量笔记应用 |
| `collaboration-room` | toeverything/collaboration-room | MPL-2.0 | 多人实时协作的服务端 |
| `affine-i18n-resources` | toeverything/AFFiNE-i18n-resources | 未识别 | 多语言资源的组织方式 |
| `notion-sdk-js` | makenotion/notion-sdk-js | MIT | Notion 的对象模型：页面、块、数据源（data sources）、属性、视图；页面按 Markdown 读写（`GetPageMarkdown`、`UpdatePageMarkdown`）；agents、agent skills、会议记录这几组新接口；分页、错误、限流重试 |
| `notion-mcp-server` | makenotion/notion-mcp-server | MIT | 从 OpenAPI 规格自动生成 MCP 工具的做法；两个按 Markdown 读写页面的工具；鉴权 |

“未识别”表示 GitHub 没认出许可证，可能根本没有许可证文件。没有许可证的代码默认保留所有权利，只能看，不能复制。

## 读过以后的几点

- **Notion 自己也在往“面向 agent”走。** `notion-mcp-server` 的 README 写明这个本地版已经不积极维护，推荐官方托管的 Remote Notion MCP，理由是：语义检索、按 Markdown 读写页面、只返回最相关的上下文、少用 token、沿用每个用户在 Notion 里的权限。这几条和 `docs/spec.md` 第 4 节（编译上下文包、预算）、第 6 节（令牌定范围）是同一个方向。
- **本地版的工具是从 API 直接翻出来的。** 一个 API 端点一个工具，agent 要自己拼请求。官方说它“token 效率有限”。我们的 MCP 工具按 agent 的意图设计（`context_get` 一次拿一个包），不照搬 API。
- **Markdown 是 agent 和人之间的通用格式。** Notion 的接口专门加了按 Markdown 读写页面，AFFiNE 也能导出 Markdown。我们的导入导出（`docs/spec.md` 6.3）走 Markdown 是对的。
- **Notion 的数据库已经拆成“数据源”和“视图”。** 数据源是带属性的集合，视图是筛选和排列。我们“条目是数据，页面只是排版”的做法和它一致。

## 规则

- 只参考设计，不把代码复制进 `crates/` 或别的地方。确实要复用某段 MIT 代码时，单独开 PR，连同原许可证和版权声明一起放进 `third_party/`，并在 PR 里写明出处。
- 不删、不改任何参考代码的许可证和版权声明。
- AFFiNE 企业版许可的部分（`packages/backend`、`packages/common/native`）禁止再分发，也不复制。
