# 用 Rust 重写 AFFiNE 内核：方案

> 2026-11-24 写。同一天维护者在卡片上选了范围甲（第 7 节第 1 条）；第 2 到 4 条没单独问，先照推荐做，维护者随时可以改。维护者的原话是“affine 的内核我想用 rust 重写”。
> 依据是 `reference/affine` 固定的那个提交（AFFiNE 0.27.5，`3d25727`）和 crates.io 上 2026-11-24 查到的许可证。企业版许可的目录只数了文件和行数、看了模块名，没读实现，也没复制任何代码；没有跑过 AFFiNE。

## 1. 结论先说

- **AFFiNE 自己已经把服务端的核心往 Rust 搬了一大半**，但搬过去的代码是企业版许可，我们不能用。浏览器里的编辑器和文档模型还是 TypeScript 加 yjs，没有用 Rust。
- 所以对我们有意义的“用 Rust 重写内核”是：**写一个 MIT 许可的、不带界面的 AFFiNE 文档内核**（块模型、工作区结构、导入导出、同步协议客户端、本地存储），给 TCMS、I.P. 的岗位和别的 agent 用。AFFiNE 的前端编辑器不动。
- 推荐范围是第 3 节的甲：约 6 个 PR、两位工程师 5 到 7 周（含 Claude 写代码），其中同步客户端就是 TCMS 正在做的 A2，不重复做。
- 推荐**先放在 TCMS 仓库里**，crate 名字不带 `tcms-` 前缀、不依赖 TCMS 别的 crate，以后要单独发布再连历史一起拆成新仓库（拆的时候再问）。
- 再往前一步（乙：写一个能让 AFFiNE 原版前端连上的 Rust 服务端，替掉企业版后端）可行但大得多，约 3 到 4 个月，而且要一直跟着 AFFiNE 前端升级。建议甲做完再决定。

## 2. AFFiNE 0.27.5 里哪些已经是 Rust

### 2.1 仓库里的 Rust

根目录 `Cargo.toml` 的工作区有 7 个 crate：

| 目录 | crate | 许可 | 行数（.rs） | 做什么 |
| --- | --- | --- | --- | --- |
| `packages/backend/native` | `affine_server_native` | **企业版** | 约 9.3 万 | 服务端的“后端运行时”：文档写入、更新压实、全文和向量检索、对象存储、会话、权限、配额、付费、AI。服务端 TypeScript 通过 N-API 调它 |
| `packages/common/native` | `affine_common` | **企业版** | 约 240 | hashcash（防刷的工作量证明） |
| `packages/frontend/native` | `affine_native` | MIT | 约 1.1 万 | 桌面端（Electron）的本地模块：磁盘同步、导入、预览、工作区元数据 |
| `packages/frontend/native/nbstore` | `affine_nbstore` | MIT | 约 4300 | 桌面端的本地存储：SQLite 里存文档更新、快照、时钟、附件，本地索引 |
| `packages/frontend/native/schema`、`sqlite_v1` | `affine_schema`、`affine_sqlite_v1` | MIT | 约 1100 | 本地 SQLite 的表结构和旧版迁移 |
| `packages/frontend/mobile-native` | `affine_mobile_native` | MIT | 约 2000 | 手机端（uniffi）的存储接口 |
| `packages/frontend/native/media_capture` | `affine_media_capture` | MIT | — | 录音（macOS、Windows） |

### 2.2 它们依赖的 AFFiNE 自己发的 crate

| crate | 版本 | 许可 | 做什么 |
| --- | --- | --- | --- |
| `y-octo` | 0.4.0 | MIT | AFFiNE 自己的 Yjs 兼容 CRDT（线程安全）。服务端合并更新全走它（`core/doc/merge-updates.ts` 只剩 native 一条路） |
| `affine_doc_loader` | 0.5.0 | **AGPL-3.0** | 解析、渲染 AFFiNE 文档（转 Markdown、取文字） |
| `affine_importer` | 0.5.0 | **AGPL-3.0** | 导入规划 |
| `doc_extractor` | 0.1.1 | **AGPL-3.0** | 从 PDF、Office 这类文件里取文字 |
| `affine_preview` | 0.1.0 | **AGPL-3.0** | Mermaid、Typst 渲染 |
| `memory-indexer`、`llm_adapter` | 0.4 | **AGPL-3.0** | 内存全文索引、模型接口适配（AFFiNE 工程师个人发的） |

注意：桌面端的 `affine_native`、`affine_nbstore` 源码是 MIT，但它们链接了 AGPL 的 `affine_doc_loader`。我们参考 nbstore 的设计没问题，不能把 `affine_doc_loader` 拉进 TCMS。

### 2.3 还是 TypeScript 的部分

| 部分 | 位置 | 规模 | 许可 |
| --- | --- | --- | --- |
| 文档模型（块、schema、事务、撤销、快照转换） | `blocksuite/framework/store` | 约 8600 行，30 个文件直接用 `yjs` | MIT（在 AFFiNE 仓库里的这份；单独的 blocksuite 仓库是 MPL-2.0） |
| 编辑器框架、27 种块和它们的界面、Markdown/HTML 适配 | `blocksuite/framework/std`、`blocksuite/affine` | 约 24 万行 | MIT |
| 前端存储和同步（IndexedDB、云同步客户端、socket.io） | `packages/common/nbstore` | 约 1.7 万行 | MIT |
| 服务端外壳：GraphQL（前端用到 187 个操作）、socket.io 同步网关（11 个事件）、登录、邮件 | `packages/backend/server` | 约 12 万行 | **企业版** |

### 2.4 一点历史

AFFiNE 以前做过一个 Rust 的“内核”OctoBase（AGPL-3.0，`reference/octobase`），想让存储和同步都在 Rust 里。后来前端照旧用 yjs，只把 CRDT 那一层拆成 MIT 的 `y-octo` 用在服务端和桌面端。教训是：**浏览器里的文档模型和编辑器绑得很紧，换掉它的代价远大于收益；Rust 适合放在服务端、本地存储和不带界面的工具里。**

## 3. “内核”指什么：四种理解

| | 甲：不带界面的文档内核（推荐） | 乙：甲 + 兼容 AFFiNE 的 Rust 服务端 | 丙：把浏览器里的文档模型换成 Rust（WASM） | 丁：编辑器 |
| --- | --- | --- | --- | --- |
| 做什么 | Rust 库：块模型和全部块（28 种）的属性、工作区根文档（文档列表、文件夹、文档属性）、Markdown 和 BlockSuite 快照的导入导出、同步协议客户端、本地 SQLite 存储 | 再写一个服务端：socket.io 同步网关、文档和附件存储、登录、工作区和成员、评论，接 AFFiNE 原版前端 | 用 Rust 写 `blocksuite/framework/store` 的替代品，编译成 WASM 给编辑器用 | 重写编辑器 |
| 谁用 | TCMS 的同步、I.P. 的岗位（读写团队文档）、命令行工具 | 想要全开源、不要企业版后端的自托管用户；TCMS 也可以不再单独跑一个 AFFiNE | AFFiNE 前端 | — |
| 和 AFFiNE 的关系 | 当客户端，不改 AFFiNE | 替掉 AFFiNE 的后端，前端照用 | 要改 AFFiNE 前端 | — |
| 规模 | 约 6 个 PR，5 到 7 周 | 甲之外再 12 到 20 个 PR，3 到 4 个月 | 大，且收益不清楚（见 2.4） | 不做 |
| 许可风险 | 低：只参考 MIT 部分 | 中：服务端只能照 MIT 的客户端（`packages/common/nbstore` 的云同步实现、`packages/common/graphql` 的操作定义）反推接口，不能读企业版服务端的实现 | 低 | — |
| 维护负担 | 跟 AFFiNE 的块结构和同步协议（A2 已经要跟） | 再加上前端每次升级用到的 GraphQL 接口 | 跟编辑器 | — |

推荐甲。理由：这是唯一直接给本项目（agent 读写团队文档）用的部分；和 TCMS 的 A1、A2 大半重合，做了不浪费；不碰企业版许可；以后做乙也要先有甲。

## 4. 和 TCMS 的 A1、A2 的关系

`docs/affine-sync.md` 第 7 节把和 AFFiNE 同步分成 A1 到 A5：

| TCMS 的步 | 状态 | 在内核里对应 |
| --- | --- | --- |
| A1：`yrs` 读写 AFFiNE 的文档结构，块 ↔ 条目，Markdown 子集 ↔ 块 | 已合并（#23），K1 第一个 PR 已搬进 `crates/affine-doc` | 内核的底子，再补全块的种类 |
| A2：登录、socket.io、取文档、推更新、收广播、断线重连、GraphQL 的工作区和评论 | 已合并（#26），K3 把 `crates/tcms-affine-client` 原样改名成 `crates/affine-sync-client` | K3 就是它。**不重复写**：K3 只是把它挪进内核的 crate。根文档的文档列表、文件夹、文档属性 A2 不写，用 `affine-doc` 的 |
| A3 到 A5：同步循环、审核页、基准 | 没做 | 留在 TCMS，用内核的库 |

为了不和 A2 撞车，建议：内核这边先做 K1、K2（只动文档模型和导入导出），等 A2 合并后再做 K3 的搬迁。A2 那边只需要注意一件事：连接层别引用 `tcms-core` 之类的 TCMS 类型，以后好搬。

## 5. 甲的分步

都先用本地造的文档和 AFFiNE 仓库里 MIT 的测试快照测，K3 起对着 Docker 里自托管的 AFFiNE 测，不花模型的钱。

| 步 | 内容 | 怎么测 | 估计 |
| --- | --- | --- | --- |
| K0 | 这份方案 | — | 本 PR |
| K1 | 新 crate `affine-doc`：从 `tcms-affine` 拆出不依赖 TCMS 的部分（读写块树、Markdown 子集），`tcms-affine` 改成调它；补齐全部块（28 种）的结构（先只读，认得每种块的属性，不认得的原样留着）；工作区根文档（`meta.pages`、`folders`、`docProperties`）的读写 | A1 的 13 个测试照过；每种块各造一个文档往返；AFFiNE 的 MIT 测试快照能读 | 1 到 2 个 PR，1.5 周 |
| K2 | 导入导出：BlockSuite 快照（`.snapshot.json` 和导出的 zip）读写；Markdown 补上表格、图片引用、数据库块转成表格（只导出）；按工作区整体导出、导入 | 用 AFFiNE 自己导出的 zip 读进来再写出去，再导回 AFFiNE 看打不打得开（手动一次，记在 PR 里） | 3 个 PR（快照、导出包、Markdown 各一个），2 周 |
| K3 | 同步协议客户端 `affine-sync-client`：就是 A2 的连接层挪过来 | A2 自己的测试 | A2 合并后 1 个小 PR，2 天 |
| K4 | 本地存储：SQLite 里存更新、快照、时钟，定期合并，断网时能改、连上后补推（参考 MIT 的 `affine_nbstore` 的设计，不复制代码，不用它依赖的 AGPL crate） | 断网改、两端同时改、重启 | 1 到 2 个 PR，1.5 周 |
| K5 | 基准 `tcms bench affine-doc`：读一篇 1 千个块的文档、合并 1 万条更新、导出整个工作区；同一份数据和 `y-octo` 比一次 | 门槛写进 `architecture.md` 第 10 节 | 1 个 PR，3 天 |

### 5.1 K1 第一个 PR 做了什么（2026-11-24）

- 新 crate `crates/affine-doc`（MIT，只依赖 yrs、comrak、similar、regex、getrandom、thiserror）：A1 的 `doc`、`markdown`、`model`、`sections` 原样搬过来（用 `git mv`，历史还在）；`tcms-affine` 只剩一行 `pub use affine_doc::*`，以后 A3 的同步循环写在那里。手动对照用的环境变量改名为 `AFFINE_DOC_FIXTURE`、`AFFINE_DOC_DUMP`。
- `order`：AFFiNE 列表用的排序键。照公开的 `fractional-indexing` 算法自己写了一遍，外面再加 AFFiNE 的 `0` 加 32 个随机字符的后缀；生成的键能被 AFFiNE 再拿去当邻居。
- `workspace`：根文档 `meta` 的工作区名字和文档列表（读 BlockSuite 写的 Y.Map 形式，也读普通对象形式；认不得的字段原样留着）；工作区“数据库”的通用表（`db$<表名>`，删除标记 `$$DELETED`）；文件夹树（建文件夹、放链接、改名、挪动、连同下面一起删，规矩和 AFFiNE 前端一样：根上只放文件夹、不能挪进自己下面）；文档属性（`custom:*` 这类不认得的字段不丢）。
- 测试：原来的 13 个加上新的 11 个（排序键 6 个，其中 5 条是 AFFiNE 自己测试里写明的行为；工作区 5 个，都编码成 Yjs 更新在另一个 Doc 里应用再读）。都是自己造的数据，**还没拿真 AFFiNE 的根文档和 `db$folders` 对照过**：A2 那边对着 Docker 里的 AFFiNE 0.27.5-beta.3 核对过文件夹表的字段和排序键格式，和这里一致；拿真文档做的对照测试等 A2 合并后补。
- 还没做：27 种块的结构（K1 第二个 PR，见 5.2）。0.27.5 服务端的“工作区状态”不用接：A2 核对过，0.27.5-beta.3 的网页版用同步协议 1，文档列表和回收站还在 `meta.pages` 里（移进回收站多出 `trash: true`、`trashDate`、`updatedDate`），工作区状态要协议 2。

### 5.2 K1 第二个 PR 做了什么（2026-11-24）

- `schema`：AFFiNE 0.27.5 的全部块，数下来是 28 种（方案里写的“27 种”少数了一种），包括前端的两种 AI 块 `affine:embed-ai-chat`、`affine:transcription`。每种块记了版本号、角色、能放在哪些块下面、哪些属性是带格式的文字、是不是只在白板里、属性是不是拍平存（`affine:table` 存成 `prop:cells.<行>:<列>.text` 这样）。`doc` 写块时的版本号也改成查这张表。
- `read_tree`：不挑块，从页面块往下把整棵树读出来，白板和只在白板里显示的 note 也读。`sys:*` 和全部 `prop:*` 原样带着，嵌套的 Y.Map、Y.Array 照原样读成嵌套的值。带格式的文字保留全部属性（颜色、行内引用这类 Markdown 写不出的也在），拍平存的表格拼回嵌套的样子。
- `check_tree`：检查一棵树合不合 0.27.5 的规矩，会查出四种问题：不认得的块、版本号不对、放错了地方、该是文字的属性不是文字。打算在写完一篇文档、读回来以后跑一遍（`affine-sync.md` 5.3 第 5 条）。
- 这一步只读，不写这些块。
- 测试：新加 5 个。表格和数据库各造一个，编码成更新以后在另一个 Doc 里读。另外在本地拿 AFFiNE 仓库里 MIT 的三个测试快照（`packages/common/reader/__tests__/__fixtures__`）手动跑了一遍：两篇文档里有 14 种块，结构检查没有问题；根文档读出了工作区名字和文档列表。这样根文档的读法也和真 AFFiNE 对上了。这个对照用忽略的测试 `interop_with_affine_files` 跑，环境变量多了一个 `AFFINE_DOC_ROOT_FIXTURE`。`db$folders` 还没有真数据对照，等 A2。

### 5.3 K3 做了什么（2026-11-24）

A2（TCMS #26）合并以后，把 `crates/tcms-affine-client` 原样改名成 `crates/affine-sync-client`（`git mv`，代码不改，只改包名、引用和注释）。对着真 AFFiNE 的测试用的环境变量本来就是 `AFFINE_TEST_URL`、`AFFINE_TEST_EMAIL`、`AFFINE_TEST_PASSWORD`，不带 TCMS，不用改。A3 的同步循环照旧写在 `tcms-affine` 里，依赖改成 `affine-sync-client`。

### 5.4 K2 第一个 PR 做了什么（2026-11-24）

- `snapshot`：BlockSuite 的文档快照（AFFiNE 导出的 `.snapshot.json`）和 Yjs 文档之间互相转换（`to_snapshot`、`from_snapshot`）。
  - 文字和原样存的值（BlockSuite 叫 `Boxed`）都按快照里的标记转。别的对象在 Yjs 里存成 Y.Map，数组存成 Y.Array。
  - 白板的 `elements` 和拍平存的表格按各自的规矩转。
  - 写文字时，前一段有、这一段没有的格式明确写成 null。不这样做的话 Yjs 会让后一段沿用前一段的格式。
  - `from_snapshot` 先写进一篇临时文档，整篇写对了再应用到目标文档，所以中途出错不会留下写了一半的文档。
  - 快照里没写版本号的块，按 `schema` 表补上。
- 测试：新加 4 个。手写的快照覆盖带格式和行内引用的文字、表格、白板元素、Boxed、嵌套对象和数组，转进 Yjs 再转回来逐字节一样；另外检查了存成 Yjs 的样子、`doc::create` 写的文档转成快照再写回、坏快照不碰目标文档。
- 和真 AFFiNE 对照：AFFiNE 仓库里 MIT 的模板（`packages/frontend/templates`，onboarding 和白板模板）有 18 个快照，在本地手动跑了一遍。
  - 18 个的结构检查都没有问题。
  - 转回快照以后只有两种差别：快照里没写版本号的块被补上了版本号；JSON 里 `1.0` 写成了 `1`（在 JS 里是同一个数）。别的逐项一样。
  - 这个对照用忽略的测试 `interop_with_affine_snapshot` 跑：`AFFINE_DOC_SNAPSHOT=<文件> cargo test -p affine-doc -- --ignored`。
- 下一步：整个工作区的导出包（`.bs.zip`：快照加 `assets/` 里的附件），见 5.5。

### 5.5 K2 第二个 PR 做了什么（2026-11-24）

- `bundle`：AFFiNE“导出 → 快照”出来的 `.bs.zip` 的读写，包里的样子照 BlockSuite 的导出（只读了设计）。
  - 每篇文档一个 `<标题>-<文档 id>.snapshot.json`（两个空格缩进），图片和附件在 `assets/<blobId>.<扩展名>`。标题里的斜杠换成 `_`，免得在 zip 里多出一层目录。
  - 读的时候跳过 `__MACOSX`、`.DS_Store`、`info.json`。AFFiNE 的白板模板外面多套了一层目录（`<名字>/assets/...`），也认。解压出来的总大小超过 1 GiB 就停，按实际解出来的字节数算。
  - `export_docs`：几篇文档导出成一个包，附件内容由调用方按 `blobId` 给（以后从 AFFiNE 的附件存储取）；取不到的附件记下来，包照样导出，和 BlockSuite 一样。
  - `import_docs`：包里每篇文档换一个新的文档 id，文档之间的引用（文字里的行内引用、嵌入文档块的 `pageId`）跟着换，指向包外的不动；全部写对了才记进根文档的文档列表，有一篇不对就一篇都不记。附件由调用方传到附件存储。和 BlockSuite 不同：块 id 不换，因为每篇都是新文档，块 id 不会撞。
- 新依赖 zip 9（MIT），只开 deflate（zlib-rs），见 `architecture.md`。
- 测试：新加 6 个。导出再读回来逐项一样、附件只取一次、缺的附件记下来；导入换 id、引用跟着换、记进文档列表；`doc::create` 写的文档过一遍包，Markdown 不变；跳过杂文件、坏 JSON 和不是文档的快照报错；有一篇坏的就一篇都不导入；MIME 到扩展名。
- 和真 AFFiNE 对照：AFFiNE 仓库里 MIT 的模板有 17 个 AFFiNE 导出的 zip（onboarding 一个、白板模板 16 个），一共 18 篇文档、45 个附件，在本地手动跑了一遍忽略的测试 `interop_with_affine_bundle`（`AFFINE_DOC_BUNDLE=<文件>`）。
  - 全部读得出来，18 篇的结构检查都没有问题，附件一个不缺。
  - 每篇转进 Yjs 再转回快照，差别还是 5.4 那两种（补上版本号，`1.0` 写成 `1`）。
  - 整包导入再导出，文档和附件个数不变，`unzip -t` 检查新包没有错。
  - 还没把我们导出的包导回 AFFiNE 界面里看打不打得开。
- 下一步：Markdown 补表格、图片引用，数据库块转成表格（只导出）。

合计约 6 到 8 个 PR、5 到 7 周（两位工程师加 Claude；Claude 能写大部分代码，工程师主要读代码和对真 AFFiNE 验收）。

## 6. 乙要做什么（只列出来，先不做）

如果甲做完决定做乙，大致是：

| 步 | 内容 |
| --- | --- |
| S1 | socket.io 服务端（`socketioxide`，MIT）+ 同步网关的 11 个事件 + 文档存储（PostgreSQL，和 I.P.、TCMS 共用一个库，单独一个 schema） |
| S2 | 登录、会话、工作区、成员、邀请：前端最少要的 GraphQL 操作（`async-graphql`，MIT/Apache-2.0）。前端一共用 187 个，能打开工作区、写文档估计要 40 到 60 个 |
| S3 | 附件：本地磁盘和 S3 兼容存储 |
| S4 | 评论、文档历史、通知 |
| S5 | 拿 AFFiNE 原版前端（MIT）连上来跑一遍端到端测试；每次 AFFiNE 升级重跑 |

许可上要守的规矩：只看 MIT 的客户端代码和 `.gql` 操作定义反推接口，不读企业版服务端（`packages/backend`）的实现。AFFiNE 是否允许第三方服务端接它的前端（商标、条款），做乙之前要查。

## 7. 要维护者定的事（第 1 条已定，第 2 到 4 条先照推荐做）

1. **范围**：**已定：甲**（2026-11-24，维护者在卡片上选的）。乙等甲做完再说。
2. **放哪里**：推荐先放 TCMS 仓库（`crates/affine-doc`、`crates/affine-sync-client`，MIT，和 TCMS 一样）；要单独发布时再拆成新仓库，拆的时候问。另一个选法是现在就建新仓库，那要你同意建。
3. **CRDT 用哪个**：推荐继续用 `yrs`（A1 已经在用，社区大）；K5 拿 `y-octo` 比一次，差得多再换。`y-octo` 是 AFFiNE 自己服务端用的，和 AFFiNE 的行为最接近。
4. **和 A2 的顺序**：推荐 K1、K2 先做，K3 等 A2 合并后搬。

## 8. 许可证核对

| 东西 | 许可 | 我们能怎么用 |
| --- | --- | --- |
| AFFiNE 仓库根（除下面两个目录） | MIT | 参考设计；确实要复用某段代码时按 `reference/README.md` 的规矩放 `third_party/` 并写明出处 |
| `packages/backend`、`packages/common/native` | AFFiNE 企业版许可：生产使用要订阅，禁止复制、分发 | 不读实现，不复制 |
| AFFiNE 仓库里的 `blocksuite/` | MIT（没有自己的许可证文件，按根目录） | 同第一行 |
| 单独的 blocksuite 仓库（`reference/blocksuite`） | MPL-2.0 | 只参考设计，不复制 |
| `y-octo` | MIT | 可以依赖 |
| `yrs` | MIT | 已在用 |
| `affine_doc_loader`、`affine_importer`、`doc_extractor`、`affine_preview`、`memory-indexer`、`llm_adapter` | AGPL-3.0 | 不依赖 |
| OctoBase（`reference/octobase`） | AGPL-3.0 | 只参考设计，不复制 |
| `socketioxide`（乙才用） | MIT | 可以依赖 |
| `async-graphql`（乙才用） | MIT 或 Apache-2.0 | 可以依赖 |
