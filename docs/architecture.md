# 技术路线

> 2026-11-22 写，按维护者当天定的路线。状态：第 1 步（索引）、第 2 步（检索、上下文包）、第 3 步（MCP 服务端）、第 4 步（提议和批准）有代码，后面几步只有设计。
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
- 人新写的标题没有 id。推到远端的，服务拉下来时补上（下面第 3 节“远端”）；在服务器上直接提交的、没配远端的，`tcms assign-ids` 给整个仓库里没有 id 的分配一个，做成一个“补 id”的提交。只插入 id，文件其余部分逐字节不变。没有 id 的条目不进索引，会提醒。人改标题不影响 id。
- 两个地方用了同一个 id 时，先进索引的那条留着，另一条不进索引，并提醒。
- `teams.yaml` 里没定义的范围目录照样索引，但会提醒。
- 解析出问题时只跳过那个文件或那一条，不让整次索引失败。
- 删掉一个标题等于归档那一条。取代用 `supersedes=ctx_xxx`。
- 版本号（`revision`）由索引器维护：一条的内容哈希变了就加一，同时记下是哪个提交，所以 `context_read(id, revision)` 能取回旧版本。

## 3. 写入

**agent 提议**：`context_propose` → PostgreSQL 的 `proposals` 表，状态 `open`。不动 git。提议 id 是 `prop_` 加 ULID。检查、脱敏、找矛盾、挡乱提见 `spec.md` 第 5 节（`crates/tcms-write/src/propose.rs`）。每个令牌没处理的提议数在一个事务里数（先锁住那个令牌的行），重复的内容靠 `content_hash` 上只管 `open` 的唯一索引挡。

**人批准**：审核页或 `tcms accept <id>` → 进提交队列 → 先从远端拉（配了远端的话）→ 改文件、提交 → 推送（配了远端的话，见下面“远端”）。提交信息带上：

```text
修改：宏观数据从哪里取

<提议的理由>

Proposal: prop_01JDX5...
Proposed-by: I.P. 宏观 (tok_01JDX4...)
Accepted-by: <审核的人>
```

批准一条的步骤（`crates/tcms-write/src/accept.rs`）：

1. 排队：先拿进程里的锁（没拿到之前不占数据库连接），再在一个事务里拿 `pg_advisory_xact_lock`（键是 `tcms_commit:` 加 schema 名，别的进程也排在这里），锁住这条提议的行，确认还是 `open`。配了远端时接着从远端拉（快进或合并）；合并不了就停在这里。
2. 把索引带到 HEAD（人可能直接改过仓库），后面的比对都照着 HEAD。
3. 认一认是不是已经写进去了（做到一半断过）：新建、取代看提议 id 对应的条目在不在；修改看条目是不是已经是提议的样子；归档看是不是已经归档。是的话只记下接受。
4. 目标要还是生效的；版本号比 `base_revision` 新时停下来（`spec.md` 第 5 节），除非写明看过现在的版本。
5. 改文件（`crates/tcms-core/src/edit.rs`）：替换、插在某条后面、追加到文件末尾、删掉一条。只动那一条的文字，文件里别的字节原样不动（保留 BOM 和 CRLF）；改完重新解析一遍，核对别的条目没变、改的那条读回来和要写的一样，不一样就不写。文件里只剩前言、一级标题和空行时整个文件删掉。
6. 提交：要求 HEAD 还是第 2 步看到的那个、工作区是干净的（`ContextRepo::commit_on`），不然不提交、报错。
7. 增量索引（服务收到通知后换上），记下接受、写成的条目和提交，记进审计，事务提交，放掉锁。
8. 配了远端时推上去。推不上去只记在结果里，接受已经记下了。

任何一步出错，事务回滚，提议还是 `open`。拒绝也走同一个队列，只改状态、记审计，不动 git。

审核页上的接受、改单条交给服务里一个专门的写入线程（`crates/tcms-review/src/writer.rs`）：gix 的仓库对象不能跨线程共享，批准的那段异步代码不能放进 axum 的多线程运行时，所以这个线程自己跑一个单线程运行时，一件一件做，每件重新打开上下文仓库；用的数据库连接池另开（最多 3 个连接），和读的请求分开。拒绝只动数据库，直接在请求里做。

**晋升**（`crates/tcms-write/src/promote.rs`，规则见 `spec.md` 第 5 节）：人在审核页或 `tcms promote` 发起，做成一条 `supersede` 提议，范围换成更大的那个，标题、正文、类型、标签、复核日期照抄当时的条目，`base_revision` 记当时的版本号，相近的条目按新范围找；存提议、查重复、挡乱提和 agent 的提议走同一段代码（`propose::save`）。提议人是发起的人，`proposer_role` 空着；审核页上发起时 `proposer_token` 是审核的人的令牌 id，命令行是 `cli`。岗位属于哪个团队从索引到的那个提交里的 `teams.yaml` 查（审核页每次打开条目时只读地打开上下文仓库读一次）。批准走的是普通的取代：新条目写进新范围的默认文件，原来那条变成被取代。

**系统找晋升候选**（`crates/tcms-write/src/candidates.rs`，2026-11-24）：在内存里那份条目（`Corpus`）上算，不查数据库。岗位和岗位比，团队、项目和团队、项目比；同一类型，相近的算法和找相近的条目一样（`conflicts.rs` 的 `score`：标题共有的词数 / 词少的那边的词数，和标签的交集 / 并集，取大的，到 50 算相近）。两两比 1 万条要比约 5 千万对，所以先按（类型，标题的词或标签）建倒排，对每一条只走它的词和标签的倒排，边走边数和别的条目共有几个词、几个标签，只算至少有一个相同的。相近的用并查集连成一组，至少跨两个范围才算。要提到的范围：都是岗位、又都在 `teams.yaml` 的同一个团队里时是那个团队，别的都是全公司；那个范围里已经有和组里某一条相近的条目时不建议。结果按跨几个范围、几条排，同样的条目结果一样。审核页每次打开“可以晋升的”算一次（1 万条约 55 到 65 毫秒，第 10 节），不缓存。

**人在审核页改单条**（`crates/tcms-write/src/edit_item.rs`）：和批准排同一个队（同一把进程锁和 advisory lock），索引带到 HEAD，条目要还是生效的、版本号要和打开页面时一样，然后像批准“修改”一样只替换那一条的文字（元数据里没改的字段原样保留，比如 `owner`），提交，增量索引，记审计 `item.edit`。提交信息：

```text
人改：PMI 的口径

Item: ctx_01JDX4M6Q8...
Edited-by: 王审核（tok_01JDX4...）
```

**人直接改**：在服务器上改、或者推到远端。推到远端的，服务定时拉下来（下面“远端”），新标题补上 id，重建改动部分的索引；在服务器上直接改的，提交以后运行 `tcms reindex` 或重启服务。远端的 webhook 和本地文件监听（notify）还没做。

**只有一个写入方**：一个 schema 同一时间只处理一条提议（上面第 1 步），所以命令行和服务同时批准也不会丢提交（测试里 4 个进程、每个进程同时批准 3 条，12 个提交都在 HEAD 的历史上；去掉锁时测试会失败）。提交前先拉远端；git 合并不了时，这条提议还是待审核，等人合并，不强推。

**远端**（`crates/tcms-git/src/remote.rs`、`crates/tcms-write/src/sync.rs`，规则见 `spec.md` 6.5）：

- 调 git 命令行，不用 gix：gix 0.90 没有推送，三方合并也要自己拼。只有配了 `TCMS_REMOTE` 时才用到 git 命令行，容器镜像里有 git 和 ssh 客户端。
- 拉：`git fetch <远端> +refs/heads/<分支>`，取到 `FETCH_HEAD`。取不到时用 `git ls-remote --exit-code` 分辨“远端还没有这个分支”（退出码 2，第一次推之前）和别的错，不靠 git 输出的语言。然后比较本地 HEAD 和远端：一样、本地领先（远端是本地的祖先）就不动；本地落后或还没有提交就 `git merge --ff-only`；两边都有新提交就 `git merge --no-ff`，提交者用这次写入的人，有冲突就记下冲突的文件、`git merge --abort`，把本地 HEAD 强推到远端的旁支 `tcms/<分支>-unmerged`（只有 tcms 写它；主分支从不强推），返回“合并不了”。容器里没有编辑器、人也不一定能上服务器，所以让人在自己的克隆里合并这个旁支，推回主分支，服务器下次拉的时候就是快进。被跟踪的文件有没提交的改动时不拉。
- 推：`git push --porcelain <远端> <提交>:refs/heads/<分支>`，推具体的提交而不是 `HEAD`，从 `--porcelain` 的输出分辨推上去了、本来就是最新的、被拒（`[rejected]`）和别的错（比如受保护的分支）。
- 凭据：设了 `TCMS_GIT_TOKEN` 时，每条命令前面加 `-c credential.helper=`（清掉别处配的）和一个 shell 写的凭据助手，git 运行它时才从环境变量展开令牌，所以令牌不在 tcms 传给 git 的参数里，也不写进仓库配置。ssh 远端没设 `GIT_SSH_COMMAND` 时用 `ssh -o BatchMode=yes`，等不到密钥或主机确认就失败。
- 每条命令都带 `-c safe.directory=<仓库>`、`-c core.hooksPath=/dev/null`、`-c commit.gpgSign=false`、`-c protocol.ext.allow=never`，环境里设 `GIT_TERMINAL_PROMPT=0`、去掉 `GIT_DIR` 这类变量。网络命令 120 秒、本地命令 60 秒没做完就停下（进程跟着结束）。
- 补 id（`crates/tcms-write/src/ids.rs`）：拉完 HEAD 变了（快进或合并），先记下拉之前的 HEAD，看从它到新 HEAD 新增、改过哪些文件，只给这些文件里没有 id 的条目补 id，照着新 HEAD 做成一个提交（`补 id：远端新来的 N 条`，提交者和这次合并一样），再往下走：同步时接着更新索引、推；批准、改单条时接着照常改、提交，两个提交一起推上去。只扫拉下来的文件，不每次扫整个仓库：一次同步多花的只是比较两个提交的文件列表（拉下来新提交时才做）。补不成（比如工作区不干净）只记在结果的错里，这些条目留给 `tcms assign-ids`。人的克隆里要 `git pull` 一下才有这些 id；拉之前人又改了同一个标题下面那一行，就是一次普通的合并冲突（上面“合并不了”）。
- 拉完 HEAD 变了也不用重开仓库：gix 每次从磁盘读引用，找不到的对象会重新扫一遍包目录（测试里拉之前打开的仓库对象，拉完照样能读、能提交）。
- 服务里的定时同步交给审核页的那个写入线程（上面），和批准、改单条排同一个队；只在拉到新提交、推上去了、或者出错的情况变了时写日志。

**条目在提议之后被人改过**（`base_revision` 落后）：和 `spec.md` 第 5 节一样，要人看过合并结果再接受。

## 4. 读取

**建索引**：每来一个新提交，用 gix 算出改动的文件，只重新解析这些文件，更新 PostgreSQL 的 `items` 表和 tantivy，每改一条在 `changes` 表记一行（全局递增序号），然后 `NOTIFY tcms_changes`。

**取一份条目**：在一个只读的可重复读事务里，从 PostgreSQL 取出全部 `active` 条目、当时最大的 `changes` 序号和索引到的提交，三样对得上（`Store::snapshot`）。在这份条目上建 tantivy 索引（第 6 节）。`tcms get` 每次现取现建。

**服务里的那一份**（`crates/tcms-mcp/src/live.rs`）：`tcms serve` 常驻一份条目和检索索引。收到 `NOTIFY tcms_changes`、或者每 30 秒，查一次最大的变更序号，和手上这份不一样就整份重取重建，建好再换上；换的时候请求继续用旧的，不等。通知断了会重新订阅，断线期间漏的由定时检查补上。MCP 那边不写条目；条目的变化经过 `tcms reindex`、批准提议或在审核页改单条，这几条路写完都更新 PostgreSQL 里的索引并发通知，服务照上面的办法换上。

取条目和建索引放在一个专门的线程里（自己的单线程 tokio 运行时、自己的一个数据库连接），tantivy 也在这个线程里直接写成一个段，不起它的后台线程。这样每一份条目和索引的内存都从同一处分配，放掉以后下一次重建接着用。一开始放在 tokio 的工作线程和 tantivy 自己的线程里，glibc 每个线程一块内存区，1 万条换 5 次索引物理内存从约 80 MB 涨到约 220 MB，换 7 次约 240 MB，还在涨；改了以后换 5 次约 85 到 96 MB，稳住了（第 10 节）。

**`context_get`**（`crates/tcms-pack`）：

1. 在内存里按调用者可见的范围过滤。这一步不查数据库，行级安全（第 5 节）管不到，靠这里的过滤和测试。
2. 固定前缀照 `spec.md` 4.2 拼。
3. 剩下的用 tantivy 按任务描述打分，同分按地位、更新时间、id 排，装到预算为止，装不下的进目录。
4. 输出里带 `cursor`（取条目时的 `changes` 序号）。

**`context_changes`**：按序号查 `changes` 表，和两个提交之间的 git diff 等价，但能按范围过滤，也不用 agent 懂 git。

**订阅**：MCP 支持资源订阅。以后 agent 订阅 `ctx://changes`，服务收到 `NOTIFY` 后给订阅了相关范围的会话发更新通知。第 3 步没做：HTTP 那头是无状态的（不发会话 id），agent 用 `context_changes` 按 cursor 问。

## 5. PostgreSQL

- **版本**：PostgreSQL 15 及以上，打包服务器上用 17。第一版不要求任何扩展。
- **和 I.P. 怎么分**（2026-11-22 和 I.P. 改 PostgreSQL 的线程对过）：同一个库 `ip`；本系统用 schema `tcms`、登录角色 `tcms_app`，I.P. 用 schema `ip`、角色 `ip_app`；各自只拥有自己的 schema，对另一边没有任何权限，跨系统只走 MCP，不做跨 schema 的 SQL。打包服务器的 compose 只起一个 `postgres:17` 容器，初始化脚本建两个角色和两个 schema，密码走环境变量。`tcms_app` 没有在库里建 schema 的权限，所以连接时先查 schema 在不在，已经有了就不再 `CREATE SCHEMA`（`IF NOT EXISTS` 也会先查这个权限，2026-11-23 在 Docker 里试部署时发现）。
- **迁移记录和锁**：`tcms_app` 的 `search_path` 是 `tcms`，sqlx 的迁移记录表落在 `tcms._sqlx_migrations`。sqlx 迁移时的 advisory lock 键按库名算，I.P. 用自己的固定键，避开它。
- **通知频道**：LISTEN/NOTIFY 的频道是库级的，本系统的频道一律以 `tcms_` 开头（目前只有 `tcms_changes`），I.P. 用 `ip_` 开头。
- **表**（`spec.md` 3.4 的四张表改成下面这样，条目本身的历史交给 git）：

  | 表 | 内容 |
  | --- | --- |
  | `items` | 条目索引：id、范围、类型、地位、标题、正文、属性、状态、版本号、所在文件、最后的提交 |
  | `item_versions` | 版本号对应的提交和内容哈希 |
  | `proposals` | 提议 id（`prop_` 加 ULID）、动作、目标条目和当时的版本号、范围、类型、标题、正文、标签、复核日期、理由、证据、找到的矛盾（JSON）、内容哈希（`open` 的唯一）、抹了几处、提议的令牌和名字和岗位、状态（`open`、`accepted`、`rejected`）、提的时间、处理的时间和人和备注、写成的条目和提交 |
  | `changes` | 全局递增序号、条目 id、范围、版本号、提交、时间 |
  | `tokens` | 令牌 id（`tok_` 加 ULID）、种类（`agent` 或 `reviewer`）、名字、令牌的 SHA-256、岗位（只有 `agent` 有，约束保证）、团队、能声明的项目（`*` 是任何项目）、发的时间、过期时间、吊销时间。两种令牌互不通用：核对 MCP 的令牌只认 `agent`，审核页只认 `reviewer` |
  | `usage` | 每条被放进上下文包、被 `read` 的次数，按岗位记 |
  | `audit` | 谁在什么时候做了什么（提议、批准、拒绝、令牌增删）。第 3 步记发令牌和吊销，第 4 步加提议、接受、拒绝，第 5 步加人在审核页改单条（`item.edit`） |

- **行级安全**（2026-11-24 做了，迁移 `0005_row_security.sql`）：agent 经 MCP 查数据库的读（`context_read` 读现在这一版和旧版本，`context_changes` 查变更）都在一个只读、可重复读的事务里，开头用 `set_config(..., true)` 写上令牌能看的范围：`tcms.scopes` 是数组写法（比如 `{org,team:research,role:macro,project:q4}`），`tcms.all_projects` 是 `on` 或 `off`（令牌的项目是 `*` 时为 `on`）。只在这个事务里有效，连接放回池子以后不带到下一次。`items`、`item_versions`（按快照里那个版本当时的范围）、`changes` 上各有一条策略 `reach`，调函数 `visible_scope(scope)`：写了 `tcms.scopes` 就只放行这些范围（空数组什么都看不到），没写就不拦。没写的是服务自己的读写：建索引、取条目建检索索引、审核页、命令行，这些要看全部。
  - 是兜底：服务本身照样过滤。`context_changes` 的 SQL 里带范围条件；`context_read` 在 Rust 里再查一遍现在这一版和要的那个版本的范围，两个都要看得到（现在看不到的条目，旧版本当时看得到也不给）。`context_changes` 的 cursor 在写上范围之前取，是全部变更里最大的序号，和账号受不受行级安全管无关；变更带的标题是条目现在的标题，条目现在的范围看不到时不带。
  - 表的主人（服务的账号）也要受管，所以三张表都 `FORCE ROW LEVEL SECURITY`。超级用户和带 `BYPASSRLS` 的账号不受行级安全管，`tcms serve` 启动时查到会提醒一句，只剩服务本身的过滤。I.P. 的打包服务器和本仓库单独部署的 `deploy/docker-compose.yml`（2026-11-24 起，初始化脚本 `deploy/postgres-init/01-tcms-app.sh`）里，tcms 都用普通账号 `tcms_app` 连，它拥有 schema `tcms`，起作用；PostgreSQL 镜像建的超级用户只拿来管库、备份。老的数据卷怎么转见 `docs/deploy-docker.md`。Supabase 的 `postgres` 账号带 `BYPASSRLS`，要起作用得另建普通账号。
  - 管不到的：`context_get`、`context_search` 在服务内存里的那一份条目上过滤，不查数据库（上面第 4 节）；`context_propose` 找矛盾也用内存里的那一份；提议、令牌、用量、审计这几张表没加策略（agent 读不到这些表的内容，只写自己的提议）。
  - 测试（`crates/tcms-store/tests/row_security.rs`）建一个普通账号，只拥有自己的 schema，像部署时一样连：store 这里读单条不写范围条件，越权的是数据库拦下的；直接写不带条件的 SQL，写上范围以后三张表都只出来这些范围的行，同一个连接的下一个事务不受影响；写上范围以后看不到的行也改不了。整套测试用普通账号也跑过（本地）。CI 用的是超级用户，其他测试里行级安全不起作用，测的是服务自己的过滤。
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

- MCP（rmcp，`crates/tcms-mcp`）：`spec.md` 6.1 的工具。第 3 步有 `context_get`、`context_search`、`context_read`、`context_changes`，第 4 步加了 `context_propose`（标成不是只读、没有破坏性、重复调用结果一样）。资源 `ctx://item/<id>`、`ctx://changes` 还没做。
  - stdio（`tcms serve --stdio`）给同机、由 agent 启动的进程：令牌从环境变量 `TCMS_TOKEN` 读，启动时核对一次，以后每次调用再核对（吊销马上生效）；标准输出只放协议消息，提示写标准错误。
  - Streamable HTTP（`tcms serve`，默认 `127.0.0.1:7341`，`/mcp`）给远程的：每个请求带 `Authorization: Bearer <令牌>`，没带或不认时直接回 401，进不了 MCP；不发会话 id，直接回 JSON，不开 SSE。默认只接受 `Host` 是本机的请求（防 DNS 重绑定），对外提供时用 `--allowed-host` 加上域名。`/healthz` 不要令牌。收到 Ctrl-C 或 SIGTERM 时不再接新连接，做完手上的请求再退出。
  - 工具出错（参数不对、`role` 和令牌不一致等）回 `isError` 和一句话，协议层的错只用于令牌不认、工具名不认识。
- 审核页（`crates/tcms-review`，第 5 步）：和 `/mcp` 在同一个 axum 服务里（`--stdio` 时没有）。
  - `/review` 是一个静态页面，`app.js`、`app.css` 编进二进制（`include_str!`），不用构建前端，不加载外部资源；`/` 跳到 `/review`。
  - 页面调 `/api/*` 的 JSON 接口（`api.rs`）：`me`、`proposals`、`proposals/{id}`（含差异、相近的条目现在的正文、默认写进的文件）、`proposals/{id}/accept`、`proposals/{id}/reject`、`due`、`items`、`items/{id}`、`items/{id}/edit`、`items/{id}/promote`（晋升，做成一条提议）、`promotions`（系统找的晋升候选，最多 50 组，组里哪一条已经有晋升提议在等审核时带上提议 id）、`remote`（和远端同步的情况，下面）。`items/{id}` 带上 `promote_to`（这一条能提到哪些范围），`proposals/{id}` 带上 `promotion`（换了范围的取代：从哪里到哪里）。每个请求带 `Authorization: Bearer <审核的人的令牌>`，每次都查库核对（吊销马上生效）。I.P. 控制台以后也可以调这些接口。
  - 令牌存在浏览器的 sessionStorage 里（关掉标签页就没了），每次请求放在请求头里，不用 cookie，所以没有跨站请求伪造的问题。页面只用 `textContent` 和建元素写内容，不拼 HTML。
  - 响应头：`Content-Security-Policy`（`default-src 'none'`，脚本、样式、请求只许同源，`frame-ancestors 'none'`）、`X-Content-Type-Options: nosniff`、`X-Frame-Options: DENY`、`Referrer-Policy: no-referrer`。和 `/mcp` 一样只接受 `Host` 是本机或 `--allowed-host` 写了的请求。
  - 出错时回 `{"error": 一句话, "code": ..., "current": 现在的版本号（版本落后时）}`：没找到 404，不是待审核、版本落后、目标没了、仓库正忙（HEAD 变了或工作区不干净）409，参数不对 400。
  - 和远端同步的情况：写入线程每次和远端打交道（定时同步、批准、改单条）以后，把结果记在内存里（`RemoteStatus`：远端、同步间隔、最近一次的时间和拉推结果、最近一次没出错的时间、合并不了时的说明；服务重启以后从头记）。`GET /api/remote` 返回它和服务器现在的时间（`now_ms`，网页拿它算“几分钟前”，不受浏览器时钟的影响）。页面登录以后、每 30 秒、每次批准或保存以后取一次，在顶上显示一行；没配远端时不显示。合并不了的说明从批准、改单条回的错误或同步结果里取，下次拉成功了就清掉。
  - 提交者是审核的人的名字，邮箱从 `TCMS_GIT_EMAIL` 读（没设时 `tcms@localhost`）；上下文仓库从 `--repo` 或 `TCMS_REPO` 读，打不开时启动时提醒，页面能看，写会失败。
- 鉴权：Bearer 令牌（`tcms_` 加 64 位十六进制，32 字节随机数），库里只存 SHA-256。令牌绑定身份（岗位、团队、能声明的项目），身份决定范围（`spec.md` 6.1）。令牌用 `tcms token add|list|revoke` 管，发和吊销记进 `audit`。

**在智能岗位服务器上**

- 一个 PostgreSQL（和 I.P. 共用，见第 5 节），`tcms` 作为 systemd 服务，上下文仓库在 `/var/lib/tcms/context`，可以配一个客户自己的远端（GitHub、Gitea 等）。
- 服务配置 `tcms.yaml`：`database_url`、上下文仓库路径、远端、监听地址、预算默认值。密钥只从环境变量读。现在还没有这个文件，用命令行参数和环境变量（`TCMS_DATABASE_URL`、`TCMS_SCHEMA`、`TCMS_REPO`、`TCMS_LISTEN`、`TCMS_ALLOWED_HOSTS`、`TCMS_TOKEN`、`TCMS_GIT_EMAIL`，远端的 `TCMS_REMOTE`、`TCMS_REMOTE_BRANCH`、`TCMS_GIT_TOKEN`、`TCMS_GIT_USERNAME`、`TCMS_SYNC_SECS`）。

**客户自己改造部署**

- 数据库换成客户自己的 PostgreSQL 或 Supabase：改 `database_url`。
- git 远端换成客户自己的：设 `TCMS_REMOTE`（`spec.md` 6.5）。
- 用容器跑：仓库根目录的 `Dockerfile`，单独部署用 `deploy/docker-compose.yml`（PostgreSQL 加 tcms），和 I.P. 一起部署用 I.P. 仓库 compose 里的 `tcms` 服务。见 `docs/deploy-docker.md`。
- 不接 MCP 的 agent：直接读上下文仓库的文件。

## 8. 以后和 AFFiNE 同步

方向已定，等第一版跑通再细化。现在的想法：一个 Markdown 文件对应一个 Yjs 文档，用 yrs 读写。人在 AFFiNE 里改，由同步程序变成提交；agent 的提议在 AFFiNE 里显示成建议。CRDT 只在这座桥上用，真相还是 git。

## 9. 依赖

都和 MIT 兼容（2026-11-22 在 crates.io 查的最新版本和许可证）。

| 用途 | crate | 许可证 |
| --- | --- | --- |
| 异步运行时 | tokio | MIT |
| PostgreSQL | sqlx 0.9 | MIT 或 Apache-2.0 |
| git | gix 0.90（纯 Rust，不依赖 libgit2）；和远端拉取、推送用 git 命令行（不是 crate，运行时要装 git） | MIT 或 Apache-2.0 |
| 全文检索 | tantivy 0.27（不开默认特性：只用内存索引，不要 mmap、压缩、词干） | MIT |
| MCP | rmcp 3.11（I.P. 用的 3.9；不开默认特性，只开服务端、stdio、Streamable HTTP 服务端，基准和测试另开客户端） | Apache-2.0 |
| HTTP | axum 0.8（只开 http1、tokio、json、query） | MIT |
| 令牌的随机数 | getrandom 0.4 | MIT 或 Apache-2.0 |
| Markdown | comrak 1.x | BSD-2-Clause |
| 文件监听 | notify 9 | CC0-1.0 |
| YAML | serde-saphyr 2（I.P. 也用它） | MIT 或 Apache-2.0 |
| 条目 id | ulid 3 | MIT |
| 内容哈希 | sha2 0.11 | MIT 或 Apache-2.0 |
| 脱敏 | regex 1 | MIT 或 Apache-2.0 |
| 审核时的差异 | similar 2 | Apache-2.0 |
| 命令行 | clap 4 | MIT 或 Apache-2.0 |
| 内存分配器 | tikv-jemallocator 0.7（带着 jemalloc 5.3 的源码，构建时用 C 编译器和 make 编；开后台线程；MSVC 上不用，用系统自带的） | MIT 或 Apache-2.0；jemalloc 本身是 BSD-2-Clause |
| 测试审核页的接口（只在测试里） | tower 0.5（util）、http-body-util 0.1 | MIT |

| 以后：Yjs | yrs 0.28 | MIT |

第 2 步之后 `Cargo.lock` 里 383 个 crate（tantivy 带进来 74 个），第 3 步之后 430 个（rmcp、axum 和它们的依赖，加上测试用的 MCP 客户端带进来的 reqwest），第 4 步之后 432 个（多了 similar 和 tcms-write；regex 之前就在），第 5 步之后 436 个（axum 的 json、query 特性带进来 serde_path_to_error、serde_urlencoded、ryu，加上 tcms-review；tower、http-body-util 之前就在），换成 jemalloc 以后 438 个（多了 tikv-jemallocator、tikv-jemalloc-sys；cc、libc 之前就在），都可以按 MIT、Apache-2.0、BSD、ISC、Zlib、Unicode 这类宽松许可用。有几个是多选一、选项里带别的许可：htmlescape（Apache-2.0 / MIT / MPL-2.0）、ryu（Apache-2.0 / BSL-1.0）、r-efi（两个版本，MIT / Apache-2.0 / LGPL-2.1），都按 MIT 或 Apache-2.0 用（ryu 按 Apache-2.0）；matchit（axum 的路由）是 MIT 加 BSD-3-Clause，两个都是宽松许可；webpki-roots 是 CDLA-Permissive-2.0（证书数据的宽松许可，第 1 步就有）。

## 10. 基准

`tcms bench` 要量的，CI 设门槛：

| 量什么 | 门槛 | 实测（2026-11-22/23，云端开发机 4 核，本地 PostgreSQL 16，release） |
| --- | --- | --- |
| 编一个 8 千 token 的上下文包（1 万条目，可见约 3700 条） | p95 < 20 毫秒 | 中位数 6.2 毫秒，p95 8.5 毫秒 |
| `context_search`（1 万条目） | p95 < 10 毫秒 | 中位数 0.3 毫秒，p95 0.8 毫秒 |
| 从 PostgreSQL 取出 1 万条生效条目 | 不设 | 68 毫秒 |
| 在 1 万条上建检索索引 | 不设 | 215 到 300 毫秒 |
| 取条目和建检索索引多用的内存（1 万条） | 不设 | 约 33 MB |
| 找一次晋升候选（1 万条，审核页打开“可以晋升的”时算） | < 250 毫秒 | 55 到 65 毫秒，20 组（2026-11-24；两两比时 245 毫秒） |
| 一个只改了一个文件的提交，到索引更新完 | < 200 毫秒 | 17 毫秒 |
| 从头重建 1 万条目的索引（空库） | < 3 秒（原估 10 秒，实测后收紧） | 288 毫秒 |
| 1 万条目、没有变更的全量索引 | 不设 | 63 毫秒 |
| 只读 git 和解析 1 万条目 | 不设 | 30 毫秒 |
| `context_get` 经过 stdio 一次往返（`tcms serve --stdio` 子进程，含核对令牌） | p95 < 30 毫秒 | 中位数 8.0 毫秒，p95 10.0 毫秒 |
| `context_search` 经过 stdio 一次往返 | p95 < 15 毫秒 | 中位数 0.9 毫秒，p95 1.6 毫秒 |
| `context_read` 经过 stdio 一次往返（带令牌范围的只读事务里查 PostgreSQL） | p95 < 15 毫秒 | 普通账号（行级安全起作用）中位数 0.7 毫秒，p95 0.9 毫秒；超级用户中位数 0.9 毫秒，p95 1.1 毫秒（2026-11-24） |
| `context_changes` 经过 stdio 一次往返（同上，从随机序号往后，默认最多 200 条） | p95 < 15 毫秒 | 普通账号中位数 2.1 毫秒，p95 2.5 毫秒；超级用户中位数 2.0 毫秒，p95 2.5 毫秒（2026-11-24） |
| 核对一次令牌（查 PostgreSQL） | p95 < 5 毫秒 | 中位数 0.13 毫秒，p95 0.17 毫秒 |
| `context_propose` 经过 stdio 一次往返（检查、找矛盾、写进 PostgreSQL） | p95 < 15 毫秒 | 中位数 1.4 毫秒，p95 2.1 毫秒 |
| 批准一条（排队、索引带到 HEAD、改文件、提交、增量索引；1 万条） | p95 < 250 毫秒 | 中位数 30 毫秒，p95 35 毫秒 |
| 服务启动（取条目、建索引、握手） | 不设 | 约 0.3 秒 |
| 加一个文件、`tcms reindex`，到服务里搜得到（整份重建） | < 2 秒 | 约 0.3 到 0.4 秒（其中 reindex 约 25 毫秒） |
| 服务处理过请求以后的物理内存（1 万条，刚处理完） | 不设 | 约 74 到 77 MB（jemalloc 还没把放掉的内存还给系统；原来用 glibc 时约 55 到 63 MB） |
| 换过 5 次索引以后的物理内存（1 万条，刚换完） | < 160 MB | 约 97 到 106 MB（同上；原来用 glibc 时约 73 到 96 MB，改成专门线程之前约 220 MB，还在涨） |
| 空闲时内存（1 万条，换过 5 次索引以后不发请求，等 20 秒以上、稳下来） | < 50 MB | 约 40 到 41 MB（原来用 glibc 时约 86 MB，等多久都不降）；1000 条约 27 MB（glibc 约 43 MB） |

`tcms bench index [--items N] [--check]` 量索引这几项，`tcms bench read [--items N] [--requests N] [--budget N] [--check]` 量读的这几项（含找一次晋升候选），`tcms bench serve [--items N] [--requests N] [--budget N] [--check]` 起一个真的 `tcms serve --stdio` 子进程、用 MCP 客户端量服务这几项（提议量完一条马上拒掉，免得撞上每个令牌 20 条的上限；最后留 10 条量批准），CI 里都带 `--check` 跑。`bench read` 造的条目内容有变化（1500 个中文两字词和 40 个英文词，常见词出现得多，每条 30 到 160 个词，约千分之三是规则、千分之二标了常驻），随机数种子固定。

加行级安全时（2026-11-24）用普通账号和超级用户各跑了一遍 `bench index`、`bench read`、`bench serve`，别的几项差别都在每次跑的波动以内（比如从头重建 1 万条 266 和 299 毫秒、取出 1 万条 75 和 69 毫秒）。CI 里的账号是超级用户，量不到行级安全的开销。

审核页（第 5 步）没有加新的基准：它的接口是人点一下调一次，不在热路径上；接受、改单条和 `tcms accept` 走同一个提交队列，批准的耗时已经在 `bench serve` 里量了。

“空闲时内存 < 50 MB”是给第 3 步的常驻服务定的。2026-11-23 把 `tcms` 的内存分配器换成 jemalloc（开后台线程）以后达到了：1 万条换过 5 次索引、空闲 20 秒以后约 40 到 41 MB。其中约 15 到 16 MB 是程序和库映射进来的代码，条目和检索索引真正在用的约 12 MB（条目的结构体约 2 MB、字符串约 7 MB，tantivy 索引约 3 MB），剩下的约 12 MB 没再细分（运行时、数据库连接等）。

原来用 glibc 的 malloc 时没达到：处理过请求以后约 55 到 63 MB，换过几次索引以后约 73 到 97 MB，等多久都不降。量下来，多出来的是建索引时用过又放掉的内存：从 PostgreSQL 取出的行、tantivy 的写入缓冲、换下来的旧索引。glibc 把它们留在进程里，夹在还在用的内存中间，还不回去。用 gdb 在空闲的进程里调一次 `malloc_trim(0)`，物理内存马上降到约 44 到 49 MB，说明只要把放掉的内存还给系统就够了。试过的：

- 改成专门线程之前，换过分配器：mimalloc 换 1 次后约 158 MB；jemalloc（没开后台线程）换 5 次约 190 MB，空闲 10 秒后回到约 112 MB。那时的问题是多个线程各占一块内存，换分配器解决不了，没用。
- glibc 的环境变量（改成专门线程以后）：`MALLOC_ARENA_MAX=1`，处理过请求后约 51 MB，换 5 次后约 84 MB；`MALLOC_MMAP_THRESHOLD_=131072`，换 5 次后约 66 MB，两个一起设约 64 MB。都不够，还要部署的人自己设，没用。
- 在代码里调 glibc 的 `malloc_trim`：要写 `unsafe` 调 C 函数（项目里禁用了 `unsafe`），降下来也刚好在门槛边上，没用。
- mimalloc 0.1（改成专门线程以后）：处理过请求后约 100 MB，换 5 次后约 120 MB，更高，没用。
- jemalloc（tikv-jemallocator 0.7，开后台线程）：刚处理完、刚换完时比 glibc 高（见上表，都在 160 MB 以内），后台线程过几秒到十几秒把放掉的内存还给系统：1 万条时 5 秒左右降到约 41 到 46 MB，15 秒左右再降到约 40 MB，之后不变。用了它。

`tcms bench serve` 加了“空闲以后的物理内存”一项：换过 5 次索引以后不发请求，至少等 20 秒，再等到 3 秒里降得不到 1 MB（最多等 40 秒），超过 50 MB 时 `--check` 失败。条目再多会超过（几万条没量过），那时按原来的想法把正文移出内存：只留检索索引和标题、属性，正文按需从 PostgreSQL 取（编一个包多一次查询，约 1 毫秒）。
