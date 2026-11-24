# 和 AFFiNE 同步：方案

> 2026-11-24 写。维护者在卡片上选了先写这份方案；同一天又在卡片上选了第 8 节 7 件事“都按推荐定”，按第 7 节分步开工。
> 依据是 `reference/affine` 固定的那个提交（AFFiNE 0.27.5，`3d25727`）：只读了 MIT 部分（`packages/common/reader`、`packages/common/nbstore`、`packages/common/graphql`、`blocksuite/`）和企业版服务端里几个接口的名字、参数，只看设计，没复制代码，也没有真的跑过 AFFiNE。
> 已定的方向（`spec.md` 第 0、8 节）：git 里的 Markdown 还是真相；AFFiNE 当人的编辑器；本系统用 `yrs` 读写 Yjs 文档；人在 AFFiNE 里改算人写的修改；agent 的提议在 AFFiNE 里显示成建议。

## 1. 做成什么样

- 人在 AFFiNE 里看、写团队上下文，像写普通文档一样；agent 照旧走本系统的 MCP，不连 AFFiNE。
- 人在 AFFiNE 里改了，几秒后变成上下文仓库里的一个提交（作者写 AFFiNE 里最后改的那个人），和人改 Markdown 推上来一样直接生效。
- 条目在本系统这边变了（提议批准、审核页改单条、有人推了 Markdown、晋升），同步程序把那几条改到 AFFiNE 的文档里，只动那几条对应的块。
- agent 的提议在 AFFiNE 里显示成文档上的评论：写提议的标题、理由和审核页的链接。批准还在审核页（或命令行），批准、拒绝以后评论标成已解决。
- 第一版不同步：白板、数据库块、图片、附件、嵌入。它们在 AFFiNE 里原样留着，git 里那一条写一行占位说明。

## 2. 读到的 AFFiNE（0.27.5）

### 2.1 文档在 Yjs 里的样子

- 一个工作区一个根文档，`meta.pages` 是所有文档的列表（id、标题、建的时间、在不在回收站）。
- 一篇文档一个 Yjs 文档。`blocks` 是块 id 到块的表，每个块是一个 Y.Map：
  - `sys:id`、`sys:flavour`（`affine:page`、`affine:note`、`affine:paragraph`、`affine:list`、`affine:code`、`affine:divider`、`affine:database`、`affine:table`、`affine:image`……）、`sys:version`、`sys:children`（子块 id 的 Y.Array）；
  - `prop:*` 是块自己的属性：`affine:page` 的 `prop:title`；`affine:paragraph` 的 `prop:type`（`text`、`h1` 到 `h6`、`quote`）和 `prop:text`（带格式的 Y.Text：粗体、斜体、删除线、行内代码、链接都是文字上的属性）；`affine:list` 的 `prop:type`（`bulleted`、`numbered`、`todo`）、`prop:checked`；`affine:code` 的 `prop:language`。
  - 一篇文档的树：`affine:page` → 一个或几个 `affine:note`（页面模式里的正文）→ 段落、列表、代码……；列表的子项是它的子块。
- 二级标题就是 `prop:type=h2` 的段落块。**块 id 不会因为改字而变**，所以“一条对应一个块”可以落实成：条目 id 对应它那个二级标题块的 id，条目的正文是这个标题块后面、下一个一、二级标题块前面的那些块。
- 工作区里另有几张表也存在 Yjs 文档里：`folders`（文件夹树：id、父 id、类型是文件夹还是文档、排序）、`docProperties`（每篇文档的自定义属性）。

### 2.2 三种接口

| 接口 | 能做什么 | 限制 |
| --- | --- | --- |
| 自带的 MCP 服务端：每个工作区一个 `/api/workspaces/<id>/mcp`，令牌是在工作区里建的 MCP 凭据 | `read_document`（返回整篇 Markdown）、`doc_search`、`create_document`、`update_document`（交整篇 Markdown，服务端按结构比较、只改有变化的块）、`update_document_meta`（改标题） | **0.27.5 的正式版只能建只读凭据**，写的工具只在开发版和 canary 里有（`mcpCredentialReadWriteAvailable`）；读回来的 Markdown 不带块 id；不支持数据库块和图片；凭据 30、90 或 365 天过期，要轮换 |
| GraphQL（`/graphql`） | `recentlyUpdatedDocs`（按更新时间列文档，带最后改的人）、评论（`createComment`、`resolveComment`、`commentChanges`）、文档历史 | 要登录的会话 |
| 同步协议：socket.io 上的几个事件 | `space:join-batch` 加入工作区、`space:load-doc`（带状态向量就只取差量）、`space:push-doc-update`（推一段 Yjs 更新）、`space:load-doc-timestamps`（哪些文档在某个时间以后变过）；服务端推 `space:broadcast-doc-updates` | 要登录的会话；加入时要报客户端版本，服务端按 semver 查（批量加入要求 ≥ 0.27.5），也就是要跟着 AFFiNE 的版本走；这是 AFFiNE 客户端自己用的协议，没有承诺对外稳定 |

登录：HTTP 认 cookie 里的会话，也认 `Authorization: Bearer <会话令牌>`；socket 握手时在 `auth` 里带 `{tokenType: "jwt", token}`。给本系统建一个专门的 AFFiNE 账号，邮箱密码放环境变量，启动时登录、换会话令牌，过期前刷新。（A2 实际用的是网页版那种 cookie 会话，见 7.2。）

## 3. 走哪条路

| | 甲：同步协议 + yrs（推荐） | 乙：AFFiNE 自带的 MCP | 丙：旁边跑一个 Node 小服务 |
| --- | --- | --- | --- |
| 做法 | 本系统自己连 socket.io，用 `yrs` 读写块；Markdown 和块之间的转换自己写（comrak 已经在用） | 读写都交整篇 Markdown，AFFiNE 自己转成块 | 用 AFFiNE 自己的 MIT 包（`@blocksuite/affine` 的 Markdown 转换、`@affine/reader`）做转换和连接，本系统通过本机 HTTP 调它 |
| 正式版能不能写 | 能 | 0.27.5 不能（只读凭据） | 能 |
| 条目和块对得上吗 | 对得上：按标题块的 id | 对不上：Markdown 里没有块 id，标题改了字只能按位置和正文猜 | 对得上 |
| 实时 | 服务端推更新，几秒内 | 只能定时问 | 几秒内 |
| 跟 AFFiNE 升级 | 要跟：客户端版本号、协议版本、块的结构变了都要改 | 基本不用 | 少：升级 npm 包 |
| 代码量 | 大，估 4 到 5 个 PR（第 7 节） | 小，2 个 PR | 中，但部署里多一个 Node 进程，和“整体用 Rust”的路线不一样 |
| 写坏 AFFiNE 文档的风险 | 有：块的结构写错了 AFFiNE 打不开。只写认得的几种块，拿 AFFiNE 自己的读取器校验 | 没有 | 小 |

推荐甲：它是 `spec.md` 第 8 节已经定的路（`yrs`，CRDT 只用在这座桥上），正式版就能读写，条目对得上块，而且是唯一不依赖 AFFiNE 还没放出的功能、又不加一种语言运行时的做法。代价是代码多、要跟 AFFiNE 的版本，用第 6 节的办法压住。

如果 AFFiNE 的读写 MCP 凭据很快会进正式版、并且读回来能带块 id（`read_document` 内部的读取器本来就有带块 id 的模式，只是 MCP 没打开），乙会更省事：可以先按甲把对应表、同步循环、冲突处理做好（这几块两条路一样），连接那一层以后换成乙。

## 4. 对应关系

| 本系统 | AFFiNE |
| --- | --- |
| 部署 | 一个专门的工作区（不和别的文档混在一起，因为能进这个工作区的人都能改团队上下文） |
| 范围（`org`、`team-research`、`role-macro`、`project-q4-report`） | 工作区根上一个同名文件夹。文件夹名用 `teams.yaml` 里的显示名，对应关系存在本系统这边，所以人改文件夹名不影响 |
| 一个 Markdown 文件 | 范围文件夹里的一篇文档，标题是文件名（不带 `.md`） |
| 一条（二级标题） | 一个 `h2` 段落块和它后面的块。条目 id 只存在本系统的对应表里，AFFiNE 里看不到 |
| 文件里第一个二级标题前面的内容（一级标题、说明） | 照样同步，不算条目（和现在的规则一样：不进索引） |
| 文件开头的 YAML（默认类型、标签、负责人、复核日期） | 不放进 AFFiNE。第一版条目的属性只在 Markdown 和审核页里看、改 |
| 条目注释里的属性（`kind`、`review_by`、`tags`……） | 同上，不放进 AFFiNE |
| agent 的提议 | 文档上的一条评论 |

转换的范围（Markdown 子集 ↔ 块）：段落、一到六级标题、引用、无序、有序和待办列表（可以嵌套）、代码块（带语言）、分隔线，行内的粗体、斜体、删除线、行内代码、链接。表格、图片、数据库、嵌入第一版不转：

- AFFiNE 里有这些块的条目，git 里在对应位置写一行占位（`<!-- affine block=<块 id> flavour=affine:image -->` 加一句“AFFiNE 里的图片，没有同步”），本系统往回写这一条时跳过这些块，不删。
- git 里带表格、图片的条目，写进 AFFiNE 时表格写成代码块、图片写成链接，并在审核页标出“在 AFFiNE 里只能看，改了不回写”。

## 5. 同步怎么跑

### 5.1 记什么

PostgreSQL 里加三张表（schema `tcms`，只有同步程序用，MCP 的令牌读不到）：

- `affine_docs`：文件路径 ↔ AFFiNE 文档 id，上次同步时这篇文档的 Yjs 状态向量、上次同步到的 git 提交、上次同步时整篇的内容哈希。
- `affine_items`：条目 id ↔（文档 id，标题块 id），上次同步时这一条的标题和正文（“底稿”，三方比较用）。
- `affine_conflicts`：两边都改了、没自动合的条目，等人处理。

### 5.2 AFFiNE → git

1. 连上以后加入工作区，订阅服务端推的更新；断线重连后用 `space:load-doc-timestamps` 补上断线期间变过的文档；另外每 5 分钟整体核对一次。
2. 一篇文档有更新时不马上写：等这篇文档 10 秒没有新的更新（人停手了）再处理，免得一个字一个提交。
3. 取这篇文档（带上次的状态向量，只拿差量），在 `yrs` 里拼出现在的块树，按标题块切成条目：
   - 标题块 id 在对应表里的：是原来那一条，比较标题和正文；
   - 新的 `h2` 块：一条新的，分配 id，类型按文件开头的默认值（没有就是 `reference`，和现在一样提醒）；
   - 对应表里有、文档里没有的：人删掉了，等于归档（和在 Markdown 里删掉标题一样）；
   - 把 `h2` 改成了别的级别：当成删掉；改回来当成新的一条。
4. 和底稿比，有变化的条目改进对应的 Markdown 文件：只改那几节，注释、文件开头的 YAML、别的条目逐字节不变；交给现有的提交队列，作者写 AFFiNE 里最后改的人（名字，邮箱用固定的 `affine@tcms.invalid`，不把 AFFiNE 账号的邮箱写进 git），提交信息末尾带 `TCMS-Sync: affine`。之后照常索引、通知、推到远端。
5. 范围文件夹里新建的文档：在 git 里那个范围目录下新建一个文件。文件夹外面的文档不管。

### 5.3 git → AFFiNE

1. 跟着变更序号走（和 `tcms serve` 换索引用的是同一个通知）：一个提交改了已经对应上的文件、并且不是同步程序自己提的（没有 `TCMS-Sync: affine`），就把这个文件里变了的条目写过去。
2. 写的时候只动那一条：改标题块的文字，删掉它后面原来的块、插入新的块。别的条目、别人正在写的地方不碰。CRDT 会把这一段改动和人同时在别处的修改合在一起。
3. 推的更新记下状态向量，服务端广播回来时认得是自己的，不再写回 git。
4. 新文件：在范围文件夹里新建文档。文件删了：把文档移进回收站（不彻底删）。
5. 写完以后用自己的读取逻辑把文档读一遍，结果要和刚写的 Markdown 一致；不一致就停下这篇文档的同步，记日志，审核页标出来。

### 5.4 两边都改了同一条

- 只有一边变了的条目照上面写到另一边。
- 两边都变了、改成一样的：只更新底稿。
- 两边都变了、改得不一样：不自动合，和提议的 `base_revision` 落后时同一个规矩（`spec.md` 第 5 节）。两边先各自保持原样，这一条记进 `affine_conflicts`，暂停这一条的同步；审核页“和 AFFiNE 同步”里并排给出 git 的、AFFiNE 的、底稿三份，人选一份或者改出合并后的再保存，保存以后两边都写成那一份。
- 同一篇文档里别的条目照常同步。

### 5.5 提议显示成评论

- 提议存进来时，目标条目（或新建时目标文件）已经对应到 AFFiNE 的，在那篇文档上发一条评论：提议的动作、标题、理由的前 200 字、审核页的链接。评论里不放正文，不放证据（可能有对话链接）。
- 批准、拒绝、被取代以后，回复一句结果，把评论标成已解决。评论 id 记在提议上。
- 人在 AFFiNE 里回复这条评论：第一版不处理，只在审核页这条提议下面列出来。

## 6. 压住风险

- **跟 AFFiNE 的版本**：配置里写支持的 AFFiNE 版本范围；连上以后先查服务端版本，不在范围里就不同步、审核页和日志里说清楚。客户端版本号报配置里的值。每升一次 AFFiNE 版本，在 Docker 里起那个版本的自托管 AFFiNE 跑一遍集成测试（不花模型的钱）。
- **写坏文档**：只写第 4 节那几种块，`sys:version` 照 AFFiNE 当前版本的块定义；不认得的块不删不改；写完读回来校验（5.3 第 5 条）；集成测试里写完用 AFFiNE 自己的读取器（`packages/common/reader`，MIT，只在集成测试里用，不进发布）再读一遍对照。
- **误删**：AFFiNE 里删掉一条在 git 里是归档，能从历史里找回；一次同步里一篇文档要归档一半以上的条目时停下来，等人在审核页确认。
- **权限**：能进这个 AFFiNE 工作区的人都能改所有范围的团队上下文，和“能连上审核页的人看得到全部”（`spec.md` 3.2）一样宽。所以要专门的工作区，只邀请该改团队上下文的人。AFFiNE 的文档级权限第一版不映射。
- **密钥**：专门账号的邮箱、密码只放环境变量（`TCMS_AFFINE_EMAIL`、`TCMS_AFFINE_PASSWORD`），会话令牌只在内存里。
- **socket.io 客户端**：crates.io 上的 `rust_socketio` 0.6 最后一次发布是 2024 年 4 月。用到的只有握手、发事件、等回执、收广播、心跳，在 `tokio-tungstenite`（MIT）上自己写了这一小层（A2，7.2），不依赖它。

## 7. 分几步做

都先用本地造的 Yjs 文档和 Docker 里自托管的 AFFiNE 测，不花模型的钱。

| 步 | 内容 | 怎么测 |
| --- | --- | --- |
| A1 | 新 crate `tcms-affine`：`yrs` 读写 AFFiNE 的文档结构；块树 ↔ 条目（按标题块切）；Markdown 子集 ↔ 块。**2026-11-24 已做**，见下面 7.1 | 单元测试：自己造的文档往返一遍不变；不认得的块原样留着 |
| A2 | 连 AFFiNE：登录、刷新会话、socket.io 加入、取文档、推更新、收广播、断线重连；GraphQL 的评论（文件夹其实在工作区的一篇 Yjs 文档里，见 7.2）。**2026-11-24 已做**，见下面 7.2 | 对着 Docker 里的自托管 AFFiNE 测；用 AFFiNE 自己的编辑器打开对照 |
| A3 | 同步循环：三张表、AFFiNE → git、git → AFFiNE、防回声、两边都改了的记下来；`tcms affine link`（把一个范围或文件接上）、`tcms affine status` | 集成测试：两边轮流改、同时改、断线、删一半 |
| A4 | 审核页“和 AFFiNE 同步”：连接状态、冲突并排对照和处理、只读的条目；提议显示成评论 | 无头浏览器点一遍 |
| A5 | 基准 `tcms bench affine`：1 千条的工作区第一次接上要多久、改一条从 AFFiNE 到提交要多久、常驻内存多多少 | 门槛定在 `architecture.md` 第 10 节 |

### 7.1 A1 做了什么（2026-11-24）

代码原来在 `crates/tcms-affine`，2026-11-24 搬进了不依赖本系统的 `crates/affine-doc`（`affine-kernel-rust.md` 的 K1），`tcms-affine` 原样转出。它只是一个库：不连网络、不碰上下文仓库和数据库，A3 的同步循环才用它。

- 读（`read`）：从页面块往下走，把不是只在白板里显示的 note（`prop:displayMode` 不是 `edgeless`）里的块接起来。认得的块是段落（正文、h1–h6、引用）、列表（圆点、编号、待办）、代码、分隔线；行内格式认粗体、斜体、删除线、行内代码、链接。别的块（图片、表格、数据库、嵌入……）变成“不转换的块”，在 Markdown 里写成一行占位注释 `<!-- affine block=<id> flavour=<flavour> 这一块只在 AFFiNE 里，没有同步 -->`。AFFiNE 有、Markdown 写不出来的东西（下划线、颜色这类别的格式，行内嵌入，段落下面的子块，折叠列表）照样读出文字，但这一块标成“转换有损失”。
- 切条目（`split`）：二级标题开始一条，到下一个一级或二级标题为止；第一个二级标题前面的和一级标题不算条目。每一条带着它那个二级标题块的 id（和上下文仓库里条目 id 的对应表 `affine_items` 在 A3 做）。
- Markdown ↔ 块（`to_blocks`、`to_markdown`）：只转上面那几种；Markdown 里这一版不转的写法（表格、图片、HTML）留下原文，标成有损失。写出来的 Markdown 读回来是同样的块：像 Markdown 记号的字会转义，粗体前后的空格挪到记号外面，中文标点挨着粗体也能读回来（comrak 的 `cjk_friendly_emphasis`）。
- 写（`create`、`set_title`、`update_item`、`insert_item`、`remove_item`）：新文档写成页面块、白板块、一个 note，块的 `sys:version` 和 0.27.5 一样，只写必要的属性（别的由 BlockSuite 加载时补默认值）。改一条时标题块的 id 不变，正文按块比较（Myers 差异，按每一块单独转成的 Markdown 比），只动变了的块：没变的块连同 Markdown 写不出的格式原样留着；占位注释对应的块不删、可以跟着挪位置；空段落留在原处。插文字一律带上格式属性（Yjs 里不带属性插进去会沿用前一个字的格式）。
- 客户端 id：AFFiNE 用的 yjs 13.6 是 32 位，yrs 打开 `small-client` 特性一样是 32 位。块 id 是 10 个字符的随机串，和 AFFiNE 自己生成的样子一样。

测试：`cargo test -p affine-doc` 里原来 A1 的 13 个单元测试，都是自己造的文档：Markdown 子集往返、转义、中文标点、不转换的块、写进去再读出来（也编码成 Yjs 更新再应用一遍）、按条目切、改一条只动变了的块、插入删除条目、占位注释、Markdown 写不出的格式不丢、空段落、嵌套列表和只在白板里的 note。

另外在临时目录里手动对照过两样，没有放进 CI（要 Node 和 AFFiNE 的仓库）：

- Rust 写的文档编码成更新，用 yjs 13.6.21 读：块的种类、`sys:version`、`sys:children`、带格式的文字、32 位客户端 id 都对；在 JS 里加一个块，Rust 读回来也对。
- AFFiNE 仓库里 MIT 许可的读取器测试用的文档快照（`packages/common/reader/__tests__/__fixtures__/test-doc.snapshot.bin`）：读出 4 条，里面的数据库块变成占位注释。

这个手动对照留了一个忽略的测试 `interop_with_affine_files`：设 `AFFINE_DOC_FIXTURE=<快照文件>` 读一个 AFFiNE 文档打印 Markdown，设 `AFFINE_DOC_DUMP=<文件>` 把样例文档写成 Yjs 更新，`cargo test -p affine-doc -- --ignored` 跑。当时还没对着真的 AFFiNE 服务跑过；A2 时对着 0.27.5-beta.3 的编辑器打开过 A1 写的文档，显示正常（7.2）。没加基准：A1 不在任何热路径上，基准在 A5。

### 7.2 A2 做了什么（2026-11-24）

代码在新 crate `crates/tcms-affine-client`。它只认 AFFiNE 的协议，收发的是 Yjs 更新的字节，不用 TCMS 自己的类型，测试里也只用 `affine-doc`（以后要整体搬进 AFFiNE 内核方案里的 `affine-sync-client`）。

- **连接**：TCP，`https` 时接 TLS（rustls 加 ring，Mozilla 的根证书，自托管用自己的 CA 时可以再给一个 PEM 文件）。HTTP 一次一个连接（hyper），WebSocket 用 tungstenite。不支持走 HTTP 代理。
- **登录**：`POST /api/auth/sign-in` 用邮箱密码登录，拿网页版那种 cookie 会话（`affine_session`、`affine_user_id`，0.27.5-beta.3 的会话 15 天过期）。以后 HTTP 和 WebSocket 握手都带这个 cookie，还带 `x-affine-version`。7 天重新登录一次；服务端说没登录时马上重新登录再试一次。原生 App 用的那条路（换 JWT、`/api/auth/session/refresh`）没用：要装成原生客户端，对一个后台账号没好处。AFFiNE 对登录限流，连着登录失败（比如密码错）会被拒一阵子（`TOO_MANY_REQUEST`），这算“过一会儿再试”的错。
- **客户端版本**：不配就用服务端 `/info` 里的版本，和这个服务端自带的网页版报的一样，AFFiNE 升级时不用跟着改。
- **socket.io**：自己写了一小层（Engine.IO 4、Socket.IO 5，只走 WebSocket，只用默认命名空间，只收发 JSON）：握手、回服务端心跳、按编号等回执、收服务端推的事件。隔 `pingInterval + pingTimeout` 没收到心跳就算断了。
- **同步协议**（协议版本报 1，和 0.27.5 的网页版一样）：
  - `join`：一次最多 100 篇。**加入了的文档才能取、才能推修改、才收得到别人改的广播**；新建文档（推的时候写 `intent: create`）也要先加入工作区。客户端会自己先加入。
  - `load_doc`：给状态向量就只回缺的那段；服务端没有这篇时是 `None`。
  - `push_update`：推一段 Yjs 更新，回服务端的时间戳。
  - `doc_timestamps`：工作区里每篇文档最后改的时间，可以只要某个时间以后的。
  - 服务端推来的：别人改了加入了的文档（带改的人的用户 id；服务端可能把几段合成一段，还是普通的 Yjs 更新）；工作区里每有人推修改，所有加入了这个工作区的连接还都会收到一个“有文档变了”（带时间戳，包括没加入的、刚新建的文档），A3 用它发现人在 AFFiNE 里新建的文档。
  - 断线以后后台自己重连（1 秒起，每次翻倍，最多 30 秒），要的话重新登录，重新加入断之前加入了的文档，再告诉用的一方“又连上了”：断开的这段时间里的修改收不到广播，A3 要拿状态向量各取一次差量。断开的时候调用直接返回“断了”，不排队。
- **协议版本 2**：服务端还有只在版本 2 能用的 `space:load-workspace-state`、`space:doc-lifecycle`（服务端自己记文档列表和回收站）。0.27.5-beta.3 的网页版没用（报的是 1），用版本 1 调它们会被拒，所以这里也不用。**文档列表和回收站照旧在工作区根文档的 `meta.pages` 里**：在网页版里把一篇文档移进回收站，根文档里那一项多了 `trash: true` 和 `trashDate`（2026-11-24 用无头浏览器试过）。
- **文件夹**：不在 GraphQL 里，在工作区里一篇叫 `db$folders` 的 Yjs 文档里：根上每个以节点 id 为键的 Y.Map 是一行，字段 `id`、`parentId`、`data`（文件夹名或文档 id）、`type`（`folder`、`doc`、`tag`、`collection`）、`index`（排序键：fractional-indexing 的键加 `0` 再加 32 个随机字符），删掉的行带 `$$DELETED`。用同步协议取、推；读写这几张表和根文档 `meta.pages` 的代码在 `affine-doc` 的 `workspace` 里（AFFiNE 内核方案的 K1，`docs/affine-kernel-rust.md`），这里不重复写。
- **GraphQL**：工作区（列出、新建）、最近改过的文档（`recentlyUpdatedDocs`）、评论（列出、发、标成已解决、删、按游标看变化）。评论的内容是一小篇 BlockSuite 文档的快照，`comment_content` 把几段纯文字做成这个样子，`comment_text` 反过来取文字。

测试：

- `cargo test -p tcms-affine-client` 的单元测试对着本机起的一个假 AFFiNE（只会登录、`/info` 和几个 socket.io 事件）：握手、加入（不重复加入）、取（带状态向量、文档不存在）、推、时间戳、广播、回心跳、断掉以后重连并重新加入、密码不对、连不上。CI 里跑这些。
- `tests/affine.rs` 对着真的 AFFiNE：新建工作区，推根文档（文档列表用 `affine-doc` 的 `workspace` 写）和一篇用 `affine-doc` 写的文档，读回来一样；另一条连接收到这条连接推的修改，改的人对；`doc_timestamps`；断开重连以后还收得到别人改的；拿状态向量取差量以后什么都不缺；GraphQL 的工作区、最近改过的文档、评论（发、列出、已解决、变化、删）；登录不上。要设 `AFFINE_TEST_URL`、`AFFINE_TEST_EMAIL`、`AFFINE_TEST_PASSWORD`，没设就跳过，CI 里没有 AFFiNE。2026-11-24 在开发机上对着 Docker 里的 `ghcr.io/toeverything/affine:0.27.5-beta.3`（加 `pgvector/pgvector:pg16`、`redis:7`，照 AFFiNE 的 `.docker/selfhost/compose.yml`）连跑 4 次都过。
- 另外手动用无头浏览器（Playwright）对照了 AFFiNE 网页版：Rust 写进去的文档在 AFFiNE 编辑器里打开，标题、二级标题、粗体、列表都对；在网页编辑器里打的字，Rust 这边收到广播、读成 Markdown 也对；连着的时候重启 AFFiNE 容器，4 秒后自己连回来。这些没写成自动测试（要浏览器和 AFFiNE）。

没做的：网页版里评论要挂在文档里某段文字上才显示在那段旁边（文字上的 `comment-<id>` 属性），这是 A4 的事；没加基准，A2 不在热路径上，基准在 A5。

## 8. 已定的事（2026-11-24，维护者在卡片上选了“都按推荐定”）

1. **走哪条路**：甲，同步协议加 `yrs`。如果 AFFiNE 的读写 MCP 凭据进了正式版、读回来能带块 id，对应表和同步循环不动，连接那一层可以换成乙，到时候再问。
2. **范围怎么放**：一个专门的工作区，一个范围一个文件夹，一个文件一篇文档。
3. **条目的属性**（类型、复核日期、标签、负责人）：第一版不放进 AFFiNE，只在 Markdown 和审核页里改；以后可以用文档的自定义属性或者标题块下面一行小字。
4. **AFFiNE 里删掉一条**：照 Markdown 的规矩算归档，一次删掉一篇文档一半以上的条目时停下来等人确认。
5. **两边都改了同一条**：不自动合，暂停这一条，在审核页并排给人选（第 5.4 节）。
6. **提议显示成评论**：做（A4），评论里只放动作、标题、理由的开头和审核页链接。
7. **先支持哪个 AFFiNE 版本**：从 0.27.5 起，自托管版（2026-11-24 查：0.27.5 还只有测试版 `0.27.5-beta.3`，正式镜像 `stable` 还是 0.27.4；同步协议的批量加入要 0.27.5，所以先对着测试版测，正式版出来再跑一遍）。AFFiNE Cloud 上的工作区能不能接、条款允不允许第三方客户端走同步协议，还没查，接 Cloud 之前先查。
