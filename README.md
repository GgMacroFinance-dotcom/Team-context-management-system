# Team Context Management System

面向 agent 的团队上下文系统：把一个团队的规则、决定、术语、流程和资料存成可引用的条目，按岗位和任务编译成上下文包交给 agent，agent 的新发现以提议的形式回来，由人批准。

需求和接口见 `docs/spec.md`，技术路线见 `docs/architecture.md`。

## 现在能做什么

第 1、2 步（`docs/spec.md` 第 11 节）：上下文仓库（git 里的 Markdown）同步进 PostgreSQL 的条目索引；按岗位和任务编上下文包、全文检索（中文按相邻两字切分）。MCP 服务端、提议、审核页还没有。

```sh
cargo build --release
export TCMS_DATABASE_URL=postgres://用户:密码@localhost/库名   # 只从环境变量读
export TCMS_REPO=/path/to/context                              # 上下文仓库，默认当前目录

tcms import ~/Downloads/notion-export --into team-research    # 导入一个 Markdown 目录，补好 id，做成一个提交
tcms assign-ids                                               # 给人新写的、还没有 id 的条目补 id
tcms reindex                                                  # 把 HEAD 同步进索引（只解析改过的文件）
tcms reindex --full                                           # 解析所有文件；版本号保留
tcms get --role macro --task "写这周的 CPI 周报"             # 编一个上下文包（团队从 teams.yaml 查），打印出来
tcms get --role macro --project q4-report --task "..." --budget 4000 --json
tcms search "周报 口径" --role macro                          # 按这个岗位能看的范围检索；不写 --role 看全部
tcms bench index --check                                      # 索引基准
tcms bench read --check                                       # 读的基准：编包、检索
```

表放在 schema `tcms` 里（`--schema` 或 `TCMS_SCHEMA` 可改），第一次连接时自动建表。

## 开发

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
TCMS_TEST_DATABASE_URL=postgres://... cargo test --locked   # 不设时跳过连数据库的测试（CI 里必须设）
```

参考仓库在 `reference/`（git 子模块，只看设计），见 `reference/README.md`。

许可证：MIT。
