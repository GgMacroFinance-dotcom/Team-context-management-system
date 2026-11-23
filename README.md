# Team Context Management System

面向 agent 的团队上下文系统：把一个团队的规则、决定、术语、流程和资料存成可引用的条目，按岗位和任务编译成上下文包交给 agent，agent 的新发现以提议的形式回来，由人批准。

需求和接口见 `docs/spec.md`，技术路线见 `docs/architecture.md`。

## 现在能做什么

第 1 到 3 步（`docs/spec.md` 第 11 节）：上下文仓库（git 里的 Markdown）同步进 PostgreSQL 的条目索引；按岗位和任务编上下文包、全文检索（中文按相邻两字切分）；MCP 服务端（`context_get`、`context_search`、`context_read`、`context_changes`，stdio 和 Streamable HTTP，令牌定范围）。提议（`context_propose`）和审核页还没有。

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
tcms token add --name "I.P. 宏观研报" --role macro --project q4-report   # 发令牌，明文只显示这一次
tcms token list                                               # 列出令牌（不含明文）
tcms token revoke tok_...                                     # 吊销，马上生效
tcms serve                                                    # MCP 服务端：http://127.0.0.1:7341/mcp，请求带 Authorization: Bearer <令牌>
tcms serve --listen 0.0.0.0:7341 --allowed-host ctx.example.com   # 对外提供时写上域名（默认只接受 Host 是本机的请求）
TCMS_TOKEN=tcms_... tcms serve --stdio                        # 给同机、由 agent 启动的进程用
tcms bench index --check                                      # 索引基准
tcms bench read --check                                       # 读的基准：编包、检索
tcms bench serve --check                                      # 服务的基准：起 tcms serve --stdio 子进程，用 MCP 客户端量
```

服务只读：改了上下文仓库以后运行 `tcms reindex`，服务收到通知（或每 30 秒查一次）后自己换上新的检索索引。

接 MCP 客户端（stdio）的配置大致是：

```json
{
  "mcpServers": {
    "tcms": {
      "command": "tcms",
      "args": ["serve", "--stdio"],
      "env": { "TCMS_DATABASE_URL": "postgres://...", "TCMS_TOKEN": "tcms_..." }
    }
  }
}
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
