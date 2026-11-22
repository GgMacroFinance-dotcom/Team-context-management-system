# 给在这个仓库里工作的 Claude

Team Context Management System（TCMS）是一个面向 agent 的团队上下文系统，独立产品，intelligent-position（I.P.）是第一个用户，通过 MCP 接入。Rust，MIT 许可证。

## 先读这些

- `docs/spec.md`：需求、数据模型、接口规格、已定的事、要定的问题、分步实现。改接口先改它，和代码放在同一个 PR。

## 工作规则

1. 一个 PR 只做一件事，尽量小。PR 描述按 `.github/pull_request_template.md` 写。
2. 每个提交用 `git commit -s` 带上 `Signed-off-by` 一行。
3. 没在真实环境跑过的东西，不要写“已上线”“已验证”。用假模型测的，写明是假的。
4. 密钥只放环境变量，不写进仓库、PR、issue 和聊天。
5. 依赖只用与 MIT 兼容的许可。AFFiNE 服务端（企业版许可）、BlockSuite（MPL-2.0）、OctoBase（AGPL-3.0）的代码没有书面重新授权前只参考设计，不复制。
6. 性能是功能的一部分：新增热路径要有 `tcms bench` 基准。
7. 注释和文档用中文，标识符用英文。
