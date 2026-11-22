# 技术路线

> 2026-11-22 写，按维护者当天定的路线。状态：第 1 步（索引）、第 2 步（检索、上下文包）、第 3 步（MCP 服务端）有代码，后面几步只有设计。
> 讲“怎么做”；“做什么”（条目、范围、上下文包、提议、接口）见 [`spec.md`](spec.md)。

## 0. 已定的事（2026-11-22，维护者）

| 事 | 决定 |
| --- | --- |
| 真相放在哪里 | 以“git 里的 Markdown”为底（路线 B），拿数据库（路线 A）和 CRDT（路线 C）的长处补它的短处 |
| 语言 | Rust |
| 数据库 | 只用 PostgreSQL，不再支持 SQLite。兼容 Supabase，但不整套搬 Supabase |
| 和 I.P. 的关系 | 智能岗位服务器上，I.P. 和本系统共用同一个 PostgreSQL，各用各的 schema。I.P. 那边的改动在 I.P. 仓库做 |
| 部署 | 装在打包卖出的智能岗位云服务器上；客户的团队也可以自己改造、换部署方式 |
| 参考仓库 | `reference/` 里的 AFFiNE、Notion 等只看设计，代码重写 |

## 1. 总体

```text
              人                              agent（I.P. 员工、Claude Code、Codex……）
              │ 编辑器 / git push / 审核页            │ MCP（stdio、Streamable HTTP）
              ▼                                        ▼
        ┌──────────────────────── tcms 服务（一个 Rust 二进制）────────────────────────┐
        │  索引器：git 变了 → 解析改动的文件 → 更新索引                                 │
        │  编译器：身份 + 任务 + 预算 → 上下文包                                        │
        │  提交队列：人批准的提议 → 改文件 → 提交（唯一的写入方）                        │
        │  MCP 服务端、HTTP 接口、命令行                                                │
        └───────┬──────────────────────┬─────────────────────────────┬────────────────┘
                ▼                      ▼                             ▼
        上下文仓库（git）        PostgreSQL（schema tcms）        tantivy 索引目录
        定下来的条目，真相        提议、令牌和范围、变更序号、     全文检索，可随时从
                                  使用次数、审计、条目索引          git 重建
```

三样东西各管各的：

| | 放什么 | 坏了怎么办 |
| --- | --- | --- |
| 上下文仓库 | 人写的和人批准过的条目。历史、diff、回滚、blame 都靠它 | 它就是真相，要备份（推到客户自己的 git 远端） |
| PostgreSQL | git 不适合放的：待处理的提议、令牌、使用次数、变更序号、审计；外加从 git 建出来的条目索引 | 提议和审计要备份；条目索引可以从 git 重建 |
| tantivy | 全文检索索引 | 删掉，`tcms reindex` 重建 |

三条路线的长处各从哪里来：

| 长处 | 来自 | 在这里怎么拿到 |
| --- | --- | --- |
| 历史、审核、diff、agent 直接读文件 | B | 上下文仓库 |
| 并发、权限、查询、高频写入 | A | PostgreSQL：提议先进数据库，不是一条提议一个提交；权限用行级安全 |
| 实时推送变化 | A | PostgreSQL 的 LISTEN/NOTIFY，转成 MCP 的资源更新通知 |
| 多端实时协作、和 AFFiNE 同步 | C | 以后在边上用 yrs 做 Markdown 和 Yjs 文档的双向桥（第 8 节）；CRDT 不当真相 |

## 2. 上下文仓库的格式

```text
context/                    ← 一个普通的 git 仓库
├── teams.yaml              ← 团队、岗位、项目的定义，岗位属于哪个团队
├── org/                    ← 全公司
│   ├── 公司简介.md
│   └── 风格指南.md
├── team-research/          ← 团队
├── role-macro/             ← 岗位
└── project-q4-report/      ← 项目
```

- 第一层目录就是范围：`org`、`team-<名字>`、`role-<名字>`、`project-<名字>`。
- 文件开头的 YAML 写这个文件里条目的默认值：`kind`、`tags`、`owner`、`review_by`。
- 每个二级标题（`## `）是一条。标题下面第一行是一个注释，写 id 和这一条自己的属性：

  ```markdown
  ## 大客户周报用什么格式
  <!-- ctx id=ctx_01JDX4M6Q8 kind=procedure review_by=2027-03-31 -->

  正文……
  ```

- 文件里没有二级标题时，整个文件是一条，标题取一级标题，没有一级标题就用文件名。
- 注释里的值有空格时加双引号：`tags="周报, 客户"`。注释里的属性覆盖文件开头的默认值。都没写 `kind` 的按 `reference`（资料）处理，并提醒。
- 代码块、引用、列表里的 `## ` 不算标题（用 comrak 解析）。不在任何二级标题下面的内容不进索引，会提醒。
- 人新写的标题没有 id，`tcms assign-ids` 给它分配一个并做成一个“补 id”的提交（只插入 id，文件其余部分逐字节不变）；以后服务会自动做。没有 id 的条目不进索引，会提醒。人改标题不影响 id。
- 两个地方用了同一个 id 时，先进索引的那条留着，另一条不进索引，并提醒。
- `teams.yaml` 里没定义的范围目录照样索引，但会提醒。
- 解析出问题时只跳过那个文件或那一条，不让整次索引失败。
- 删掉一个标题等于归档那一条。取代用 `supersedes=ctx_xxx`。
- 版本号（`revision`）由索引器维护：一条的内容哈希变了就加一，同时记下是哪个提交，所以 `context_read(id, revision)` 能取回旧版本。

## 3. 写入

**agent 提议**：`context_propose` → PostgreSQL 的 `proposals` 表，状态 `open`。不动 git。

**人批准**：审核页或 `tcms accept <id>` → 进提交队列 → 服务改文件、提交、推送（配了远端的话）。提交信息带上：

```text
Proposal: p_01JDX5...
Proposed-by: macro-researcher-1
Accepted-by: <审核的人>
```

**人直接改**：在服务器上改、或者推到远端。服务通过远端的 webhook、定时拉取、或者本地文件监听（notify）发现新提交，拉下来，重建改动部分的索引。

**只有一个写入方**：服务里一个队列，一次只做一个提交。提交前先拉远端；git 合并不了时，这条提议回到“需要人合并”，不强推。

**条目在提议之后被人改过**（`base_revision` 落后）：和 `spec.md` 第 5 节一样，要人看过合并结果再接受。

## 4. 读取

**建索引**：每来一个新提交，用 gix 算出改动的文件，只重新解析这些文件，更新 PostgreSQL 的 `items` 表和 tantivy，每改一条在 `changes` 表记一行（全局递增序号），然后 `NOTIFY tcms_changes`。

**取一份条目**：在一个只读的可重复读事务里，从 PostgreSQL 取出全部 `active` 条目、当时最大的 `changes` 序号和索引到的提交，三样对得上（`Store::snapshot`）。在这份条目上建 tantivy 索引（第 6 节）。`tcms get` 每次现取现建。

**服务里的那一份**（`crates/tcms-mcp/src/live.rs`）：`tcms serve` 常驻一份条目和检索索引。收到 `NOTIFY tcms_changes`、或者每 30 秒，查一次最大的变更序号，和手上这份不一样就整份重取重建，建好再换上；换的时候请求继续用旧的，不等。通知断了会重新订阅，断线期间漏的由定时检查补上。服务本身不写条目，条目的变化都经过 `tcms reindex`。

取条目和建索引放在一个专门的线程里（自己的单线程 tokio 运行时、自己的一个数据库连接），tantivy 也在这个线程里直接写成一个段，不起它的后台线程。这样每一份条目和索引的内存都从同一处分配，放掉以后下一次重建接着用。一开始放在 tokio 的工作线程和 tantivy 自己的线程里，glibc 每个线程一块内存区，1 万条换 5 次索引物理内存从约 80 MB 涨到约 220 MB，换 7 次约 240 MB，还在涨；改了以后换 5 次约 85 到 96 MB，稳住了（第 10 节）。

**`context_get`**（`crates/tcms-pack`）：

1. 在内存里按调用者可见的范围过滤（数据库行级安全再拦一道，见第 5 节，还没做）。
2. 固定前缀照 `spec.md` 4.2 拼。
3. 剩下的用 tantivy 按任务描述打分，同分按地位、更新时间、id 排，装到预算为止，装不下的进目录。
4. 输出里带 `cursor`（取条目时的 `changes` 序号）。

**`context_changes`**：按序号查 `changes` 表，和两个提交之间的 git diff 等价，但能按范围过滤，也不用 agent 懂 git。

**订阅**：MCP 支持资源订阅。以后 agent 订阅 `ctx://changes`，服务收到 `NOTIFY` 后给订阅了相关范围的会话发更新通知。第 3 步没做：HTTP 那头是无状态的（不发会话 id），agent 用 `context_changes` 按 cursor 问。

## 5. PostgreSQL

- **版本**：PostgreSQL 15 及以上，打包服务器上用 17。第一版不要求任何扩展。
- **和 I.P. 怎么分**（2026-11-22 和 I.P. 改 PostgreSQL 的线程对过）：同一个库 `ip`；本系统用 schema `tcms`、登录角色 `tcms_app`，I.P. 用 schema `ip`、角色 `ip_app`；各自只拥有自己的 schema，对另一边没有任何权限，跨系统只走 MCP，不做跨 schema 的 SQL。打包服务器的 compose 只起一个 `postgres:17` 容器，初始化脚本建两个角色和两个 schema，密码走环境变量。
- **迁移记录和锁**：`tcms_app` 的 `search_path` 是 `tcms`，sqlx 的迁移记录表落在 `tcms._sqlx_migrations`。sqlx 迁移时的 advisory lock 键按库名算，I.P. 用自己的固定键，避开它。
- **通知频道**：LISTEN/NOTIFY 的频道是库级的，本系统的频道一律以 `tcms_` 开头（目前只有 `tcms_changes`），I.P. 用 `ip_` 开头。
- **表**（`spec.md` 3.4 的四张表改成下面这样，条目本身的历史交给 git）：

  | 表 | 内容 |
  | --- | --- |
  | `items` | 条目索引：id、范围、类型、地位、标题、正文、属性、状态、版本号、所在文件、最后的提交 |
  | `item_versions` | 版本号对应的提交和内容哈希 |
  | `proposals` | 同 `spec.md` 3.4 |
  | `changes` | 全局递增序号、条目 id、范围、版本号、提交、时间 |
  | `tokens` | 令牌 id（`tok_` 加 ULID）、名字、令牌的 SHA-256、岗位、团队、能声明的项目（`*` 是任何项目）、发的时间、过期时间、吊销时间。第 3 步只有给 agent 的令牌；审核的人的令牌第 5 步加 |
  | `usage` | 每条被放进上下文包、被 `read` 的次数，按岗位记 |
  | `audit` | 谁在什么时候做了什么（提议、批准、拒绝、令牌增删）。第 3 步记发令牌和吊销 |

- **行级安全**（还没做）：服务在每个事务开头写 `SET LOCAL tcms.scopes = '{org,team:research,role:macro}'`，`items`、`changes` 上的策略只放行这些范围。服务本身也过滤，这一层是兜底。第 3 步服务读 `items`、`item_versions`、`changes` 时只在代码里过滤（`context_read`、`context_changes` 按令牌的范围，测试里核对了越权读和不存在回答一样）。
- **迁移**：sqlx 的迁移文件（`crates/tcms-store/migrations/`），连接时自动跑。第一版用运行时查询（`sqlx::query`），每条 SQL 都由连真 PostgreSQL 的集成测试覆盖；以后表多了再考虑换成编译期检查的 `query!` 宏。批量写入用 `UNNEST`，一条语句写一批。
- **索引锁**：每次索引在一个事务里，开头拿 `pg_advisory_xact_lock`（键按 schema 名算），并确认库里记的索引提交没被别的进程改过，改过就重来一次。
- **兼容 Supabase**：只用标准 PostgreSQL，客户把 `DATABASE_URL` 指向 Supabase 就能跑，Supabase Studio 能直接看 `tcms` 的表。不依赖 Supabase 的鉴权、PostgREST、Realtime、Storage。
- **以后**：语义检索用 pgvector（PostgreSQL 许可证），做成可选，不开就不需要这个扩展。要开时打包镜像换成带 pgvector 的 PostgreSQL 17 镜像。

## 6. 检索

代码在 `crates/tcms-search`。

- tantivy，放在进程里。**第一版的索引建在内存里，不落盘**（原写“索引目录在数据目录下”）：从 PostgreSQL 取出全部生效条目，在当前线程里按 id 顺序直接写成一个段（tantivy 的 `single_segment_index_writer`，不起后台线程；这个接口标了 `doc(hidden)`，升级 tantivy 时要看它还在不在）。1 万条建一次约 0.2 到 0.3 秒，多用约 33 MB 内存（第 10 节）。条目多到启动明显变慢时再改成落盘。
- 分词：英文和数字按词、转小写，超过 64 字节的词跳过；中日韩文字按相邻两字（bigram），一段只有一个字时就是这个字；标点和空白只当分隔。不用词典。I.P. 的 `ip-store` 也是这么做的，当时量过 jieba-rs 要 33 MB 内存、加载 97 毫秒，所以没用。
- 字段：标题（权重 3）、标签（2）、正文（1），只记词频不记位置。范围、类型的过滤在内存里按条目做，不进 tantivy；索引里只有一个 `ord` 快速字段，指回内存里的条目。
- 查询：任务描述用同样的规则切词，去重后按出现顺序最多取 256 个；每个词在三个字段上各查一次，分数相加。收集所有命中的文档，不用 tantivy 的 top-k 剪枝，结果和段的布局无关。
- 排序：BM25 分数乘 1000 取整，分数高的在前，同分按地位、更新时间、id。同样的条目和请求，结果逐字节相同（测试里把条目倒着写进索引、从两个 schema 各建一次，比对输出）。
- 一个段：现在一次建完，段数不影响分数（测试里用普通的写入器分几次提交造出多个段再合并，和直接写成一个段的结果比对）；以后增量更新有删除时，tantivy 在合并前还把删掉的文档算进统计量，合成一个段才和重建的一样。
- 词的稀有程度（IDF）按全部条目算，包括调用者看不到的范围。看不到的条目不会出现在结果里，但会轻微影响排序。
- `context_search` 返回 id、版本、类型、范围、标题和一段摘录：正文里第一个命中词前面留 30 字，一共 120 字，换行换成空格；正文没命中时取开头。

## 7. 接口和部署

**接口**

- MCP（rmcp，`crates/tcms-mcp`）：`spec.md` 6.1 的工具。第 3 步有 `context_get`、`context_search`、`context_read`、`context_changes`，`context_propose` 第 4 步和提议一起做。资源 `ctx://item/<id>`、`ctx://changes` 还没做。
  - stdio（`tcms serve --stdio`）给同机、由 agent 启动的进程：令牌从环境变量 `TCMS_TOKEN` 读，启动时核对一次，以后每次调用再核对（吊销马上生效）；标准输出只放协议消息，提示写标准错误。
  - Streamable HTTP（`tcms serve`，默认 `127.0.0.1:7341`，`/mcp`）给远程的：每个请求带 `Authorization: Bearer <令牌>`，没带或不认时直接回 401，进不了 MCP；不发会话 id，直接回 JSON，不开 SSE。默认只接受 `Host` 是本机的请求（防 DNS 重绑定），对外提供时用 `--allowed-host` 加上域名。`/healthz` 不要令牌。收到 Ctrl-C 或 SIGTERM 时不再接新连接，做完手上的请求再退出。
  - 工具出错（参数不对、`role` 和令牌不一致等）回 `isError` 和一句话，协议层的错只用于令牌不认、工具名不认识。
- HTTP（axum）：以后给审核页和 I.P. 控制台用的 JSON 接口和 `/mcp` 放在同一个服务里（第 5 步）。
- 鉴权：Bearer 令牌（`tcms_` 加 64 位十六进制，32 字节随机数），库里只存 SHA-256。令牌绑定身份（岗位、团队、能声明的项目），身份决定范围（`spec.md` 6.1）。令牌用 `tcms token add|list|revoke` 管，发和吊销记进 `audit`。

**在智能岗位服务器上**

- 一个 PostgreSQL（和 I.P. 共用，见第 5 节），`tcms` 作为 systemd 服务，上下文仓库在 `/var/lib/tcms/context`，可以配一个客户自己的远端（GitHub、Gitea 等）。
- 服务配置 `tcms.yaml`：`database_url`、上下文仓库路径、远端、监听地址、预算默认值。密钥只从环境变量读。第 3 步还没有这个文件，用命令行参数和环境变量（`TCMS_DATABASE_URL`、`TCMS_SCHEMA`、`TCMS_LISTEN`、`TCMS_ALLOWED_HOSTS`、`TCMS_TOKEN`）。

**客户自己改造部署**

- 数据库换成客户自己的 PostgreSQL 或 Supabase：改 `database_url`。
- git 远端换成客户自己的：改配置。
- 用容器跑：官方给一个 Dockerfile 和 compose 示例（PostgreSQL 加 tcms）。
- 不接 MCP 的 agent：直接读上下文仓库的文件。

## 8. 以后和 AFFiNE 同步

方向已定，等第一版跑通再细化。现在的想法：一个 Markdown 文件对应一个 Yjs 文档，用 yrs 读写。人在 AFFiNE 里改，由同步程序变成提交；agent 的提议在 AFFiNE 里显示成建议。CRDT 只在这座桥上用，真相还是 git。

## 9. 依赖

都和 MIT 兼容（2026-11-22 在 crates.io 查的最新版本和许可证）。

| 用途 | crate | 许可证 |
| --- | --- | --- |
| 异步运行时 | tokio | MIT |
| PostgreSQL | sqlx 0.9 | MIT 或 Apache-2.0 |
| git | gix 0.90（纯 Rust，不依赖 libgit2） | MIT 或 Apache-2.0 |
| 全文检索 | tantivy 0.27（不开默认特性：只用内存索引，不要 mmap、压缩、词干） | MIT |
| MCP | rmcp 3.11（I.P. 用的 3.9；不开默认特性，只开服务端、stdio、Streamable HTTP 服务端，基准和测试另开客户端） | Apache-2.0 |
| HTTP | axum 0.8（只开 http1、tokio） | MIT |
| 令牌的随机数 | getrandom 0.4 | MIT 或 Apache-2.0 |
| Markdown | comrak 1.x | BSD-2-Clause |
| 文件监听 | notify 9 | CC0-1.0 |
| YAML | serde-saphyr 2（I.P. 也用它） | MIT 或 Apache-2.0 |
| 条目 id | ulid 3 | MIT |
| 内容哈希 | sha2 0.11 | MIT 或 Apache-2.0 |
| 命令行 | clap 4 | MIT 或 Apache-2.0 |

| 以后：Yjs | yrs 0.28 | MIT |

第 2 步之后 `Cargo.lock` 里 383 个 crate（tantivy 带进来 74 个），第 3 步之后 430 个（rmcp、axum 和它们的依赖，加上测试用的 MCP 客户端带进来的 reqwest），都可以按 MIT、Apache-2.0、BSD、ISC、Zlib、Unicode 这类宽松许可用。有几个是多选一、选项里带别的许可：htmlescape（Apache-2.0 / MIT / MPL-2.0）、r-efi（两个版本，MIT / Apache-2.0 / LGPL-2.1），都按 MIT 或 Apache-2.0 用；matchit（axum 的路由）是 MIT 加 BSD-3-Clause，两个都是宽松许可；webpki-roots 是 CDLA-Permissive-2.0（证书数据的宽松许可，第 1 步就有）。

## 10. 基准

`tcms bench` 要量的，CI 设门槛：

| 量什么 | 门槛 | 实测（2026-11-22/23，云端开发机 4 核，本地 PostgreSQL 16，release） |
| --- | --- | --- |
| 编一个 8 千 token 的上下文包（1 万条目，可见约 3700 条） | p95 < 20 毫秒 | 中位数 6.2 毫秒，p95 8.5 毫秒 |
| `context_search`（1 万条目） | p95 < 10 毫秒 | 中位数 0.3 毫秒，p95 0.8 毫秒 |
| 从 PostgreSQL 取出 1 万条生效条目 | 不设 | 68 毫秒 |
| 在 1 万条上建检索索引 | 不设 | 215 到 300 毫秒 |
| 取条目和建检索索引多用的内存（1 万条） | 不设 | 约 33 MB |
| 一个只改了一个文件的提交，到索引更新完 | < 200 毫秒 | 17 毫秒 |
| 从头重建 1 万条目的索引（空库） | < 3 秒（原估 10 秒，实测后收紧） | 288 毫秒 |
| 1 万条目、没有变更的全量索引 | 不设 | 63 毫秒 |
| 只读 git 和解析 1 万条目 | 不设 | 30 毫秒 |
| `context_get` 经过 stdio 一次往返（`tcms serve --stdio` 子进程，含核对令牌） | p95 < 30 毫秒 | 中位数 8.0 毫秒，p95 10.0 毫秒 |
| `context_search` 经过 stdio 一次往返 | p95 < 15 毫秒 | 中位数 0.9 毫秒，p95 1.6 毫秒 |
| 核对一次令牌（查 PostgreSQL） | p95 < 5 毫秒 | 中位数 0.13 毫秒，p95 0.17 毫秒 |
| 服务启动（取条目、建索引、握手） | 不设 | 约 0.3 秒 |
| 加一个文件、`tcms reindex`，到服务里搜得到（整份重建） | < 2 秒 | 约 0.3 到 0.4 秒（其中 reindex 约 25 毫秒） |
| 服务处理过请求以后的物理内存（1 万条） | 不设 | 约 55 到 61 MB |
| 换过 5 次索引以后的物理内存（1 万条） | < 160 MB | 约 85 到 96 MB（修之前约 220 MB，还在涨） |
| 空闲时内存 | < 50 MB | 没达到，见下面 |

`tcms bench index [--items N] [--check]` 量索引这几项，`tcms bench read [--items N] [--requests N] [--budget N] [--check]` 量读的这几项，`tcms bench serve [--items N] [--requests N] [--budget N] [--check]` 起一个真的 `tcms serve --stdio` 子进程、用 MCP 客户端量服务这几项，CI 里都带 `--check` 跑。`bench read` 造的条目内容有变化（1500 个中文两字词和 40 个英文词，常见词出现得多，每条 30 到 160 个词，约千分之三是规则、千分之二标了常驻），随机数种子固定。

“空闲时内存 < 50 MB”是给第 3 步的常驻服务定的，**1 万条时没达到**：处理过请求以后约 55 到 61 MB，换过几次索引以后约 76 到 97 MB（重建时新旧两份同时在，放掉以后内存留在进程里给下一次用）。其中约 14 MB 是程序本身映射进来的代码，条目加检索索引约 26 到 33 MB。试过的：

- 改成专门线程之前，换过分配器：mimalloc 换 1 次后约 158 MB；jemalloc 换 5 次约 190 MB，空闲 10 秒后回到约 112 MB。都没解决上涨，没用。
- glibc 的环境变量 `MALLOC_ARENA_MAX=1`：改成专门线程以后再设，处理过请求后约 51 MB，换 5 次后约 84 MB，只低一点，没有写进默认配置。

下一步要压到 50 MB 以内，按原来的想法把正文移出内存：只留检索索引和标题、属性，正文按需从 PostgreSQL 取（编一个包多一次查询，约 1 毫秒）。单独一个 PR，等第 4 步以后再看要不要做；条目少的团队（几百、几千条）现在就在 50 MB 以内。
