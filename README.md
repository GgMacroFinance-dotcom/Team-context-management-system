# Team Context Management System

面向 agent 的团队上下文系统：把一个团队的规则、决定、术语、流程和资料存成可引用的条目，按岗位和任务编译成上下文包交给 agent，agent 的新发现以提议的形式回来，由人批准。

需求和接口见 `docs/spec.md`，技术路线见 `docs/architecture.md`。

## 现在能做什么

第 1 到 5 步（`docs/spec.md` 第 11 节）：上下文仓库（git 里的 Markdown）同步进 PostgreSQL 的条目索引；按岗位和任务编上下文包、全文检索（中文按相邻两字切分）；MCP 服务端（`context_get`、`context_search`、`context_read`、`context_changes`、`context_propose`，stdio 和 Streamable HTTP，令牌定范围，查数据库的读由 PostgreSQL 的行级安全按令牌的范围再拦一道）；agent 的提议（检查、脱敏、找相近的条目、挡乱提）由人在审核页或命令行批准后写进 git；审核页还能看待复核的条目、按范围和类型浏览、改单条的纯文本，把岗位、团队的条目提到更大的范围（晋升，做成提议再批准），并列出系统找的晋升候选（几个岗位都写了的相近条目）。配了 git 远端时，提交之前先拉、提交以后推上去，服务定时同步，配了推送通知（webhook）时人推上去马上同步（`docs/spec.md` 6.5）。

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
tcms token add --reviewer --name "王审核"                     # 给审核的人发令牌，登录审核页用（agent 的令牌登录不了）
tcms proposals                                                # 待审核的提议
tcms proposals prop_...                                       # 看一条：理由、证据、相近的条目、批准以后的差异
tcms accept prop_...                                          # 批准：改 Markdown、做成一个提交、更新索引
tcms accept prop_... --title "..." --body-file 改过的.md --note "..."   # 改了再接受
tcms accept prop_... --over 3                                 # 条目在提议之后被人改过：看过第 3 版以后照样接受
tcms reject prop_... --note "重复了"
tcms promote ctx_... --to team:research --reason "数据岗位也要用"   # 晋升：做成一条提议，批准以后才生效
tcms suggest-promotions                                       # 系统找的晋升候选：几个岗位（或团队、项目）都写了的相近条目
tcms review-due --within 30                                   # 复核日期到了（和 30 天内到期）的条目
tcms serve                                                    # MCP 服务端 http://127.0.0.1:7341/mcp（请求带 Authorization: Bearer <令牌>），审核页 http://127.0.0.1:7341/review
tcms serve --listen 0.0.0.0:7341 --allowed-host ctx.example.com   # 对外提供时写上域名（默认只接受 Host 是本机的请求）
TCMS_TOKEN=tcms_... tcms serve --stdio                        # 给同机、由 agent 启动的进程用
TCMS_REMOTE=origin tcms sync                                  # 和远端同步一次：拉、更新索引、推（https 远端的令牌放在 TCMS_GIT_TOKEN）
TCMS_REMOTE=origin TCMS_WEBHOOK_SECRET=... tcms serve        # 再接远端的推送通知 POST /hooks/push，推上去马上同步
tcms bench index --check                                      # 索引基准
tcms bench read --check                                       # 读的基准：编包、检索、找晋升候选
tcms bench serve --check                                      # 服务的基准：起 tcms serve --stdio 子进程，用 MCP 客户端量
```

agent 不直接写条目：`context_propose` 只记下提议，人在审核页或用 `tcms accept` 批准以后才写进上下文仓库。命令行批准时审核的人从 `TCMS_ACTOR` 或 `USER` 读，提交者从 `TCMS_GIT_NAME`、`TCMS_GIT_EMAIL` 读；审核页上提交者是登录的人，邮箱从 `TCMS_GIT_EMAIL` 读。人直接改了上下文仓库以后运行 `tcms reindex`。服务收到通知（或每 30 秒查一次）后自己换上新的检索索引。

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

I.P. 的员工怎么接（每个岗位一个令牌，每轮开头取一次包），见 I.P. 仓库的 `docs/ip/team-context.md`。

用 Docker 部署（镜像、单独部署的 compose、和 I.P. 一起部署）见 [`docs/deploy-docker.md`](docs/deploy-docker.md)。

表放在 schema `tcms` 里（`--schema` 或 `TCMS_SCHEMA` 可改），第一次连接时自动建表。

想先看看审核页：`TCMS_DATABASE_URL=postgres://... scripts/review-demo.sh` 造一个小上下文仓库（schema `tcms_demo`，目录 `./tcms-demo`）、发两个令牌、起服务、让 agent 提 4 条提议，然后打开它打印的地址，用它打印的令牌登录。装了 Playwright 的话，`node scripts/review-walk.js ...`（命令也是它打印的）会在无头浏览器里把审核页点一遍。

## 开发

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
TCMS_TEST_DATABASE_URL=postgres://... cargo test --locked   # 不设时跳过连数据库的测试（CI 里必须设）
```

参考仓库在 `reference/`（git 子模块，只看设计），见 `reference/README.md`。

许可证：MIT。
