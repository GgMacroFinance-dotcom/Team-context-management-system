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
- `workspace`：根文档 `meta` 的工作区名字和文档列表（读 BlockSuite 写的 Y.Map 形式，也读普通对象形式；认不得的字段原样留着）；工作区“数据库”的通用表（服务端文档 id 是 `db$<工作区 id>$<表名>`，原来写成 `db$<表名>`，A3a 对真 AFFiNE 发现后改正；删除标记 `$$DELETED`）；文件夹树（建文件夹、放链接、改名、挪动、连同下面一起删，规矩和 AFFiNE 前端一样：根上只放文件夹、不能挪进自己下面）；文档属性（`custom:*` 这类不认得的字段不丢）。
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
- 下一步：Markdown 补表格、图片引用，数据库块转成表格（只导出），见 5.6。

### 5.6 K2 第三个 PR 做了什么（2026-11-25）

- `md_export`：整篇文档导出成 Markdown，只导出，相当于 AFFiNE 的“导出 → Markdown”，写法照 BlockSuite 的 Markdown 导出（只读了设计）。从快照出发，所以文档和导出包里的快照都能导出。
  - 和 `markdown` 分开：`markdown` 是同步用的子集，要能来回转，认不得的块写成占位注释；`md_export` 尽量把每种块写成人能读的 Markdown，给导出、备份、交给模型读用，不保证转得回来。
  - 文档标题写成一级标题。表格（`affine:table`）按行、列的排序键写成 GFM 表格，第一行当表头；数据库写成表格，表头是列名，每行一个子块，单选、多选写选项名字，日期写 `yyyy-MM-dd`（按 UTC），勾选写 `✔`；格子里的换行写成 `<br>`。
  - 图片写成 `![说明](assets/<文件名>)`，附件写成 `[文件名](assets/<文件名>)`（BlockSuite 不导出附件，这里写成链接），文件名和导出包里 `assets/` 下的一样，由调用方给；找不到的记下来。
  - 公式块写成 `$$` 块、行内公式写成 `$...$`；提示块写成引用；书签和嵌入写成链接；引用别的文档写成 `[标题](<链接前缀>/<文档 id>)`，没给前缀只写标题。
  - 白板和只在白板里显示的 note 不导出；文字颜色、背景色这类写不出的格式和不认得的块记在 `lossy` 里。
- 测试：新加 4 个。文字格式、引用、行内公式、列表嵌套、代码、公式块；图片、附件、书签、嵌入文档、提示块，有没有链接前缀；表格按排序键排、数据库各种列；`doc::create` 写的文档整篇导出和同步子集的 Markdown 一样（多一个标题）。
- 和真 AFFiNE 对照：拿 5.5 的 17 个模板包跑了忽略的测试 `export_affine_bundle_to_markdown`（`AFFINE_DOC_BUNDLE=<文件>`），18 篇都导出来了，用到的图片都找得到附件。onboarding 的两篇有文字、待办、嵌套列表、表格、数据库（看板）、公式、图片，导出来的样子人工看过；白板模板只有标题，其余在白板上，按设计不导出。
- 没做：Markdown 里的表格、图片导进 AFFiNE 变成表格块、图片块。这要改同步用的块模型（`model::Content`）和 `tcms-affine` 的同步，单独再提。

### 5.7 K4 第一个 PR 做了什么（2026-11-25）

- 新 crate `crates/affine-store`（MIT）：一个工作区一个 SQLite 文件，存文档更新、合并后的快照、时钟、附件。设计参考 AFFiNE 桌面端 `affine_nbstore`（MIT，只读了设计），不依赖它链接的 AGPL crate；表结构是自己的，比它少（没有本地索引、工作区清单）。
  - 每篇文档 = 快照 + 后来的更新。`get_doc` 把它们合成一份（yrs 的 `merge_updates_v1`），同一个事务里写回快照、删掉合进去的更新。`get_doc_diff` 按对方的状态向量给差量。
  - 每篇文档一个毫秒时钟，每来一个更新往前走，一篇文档里严格递增（同一毫秒或系统时间往回拨就加一）。
  - 对每个对方（比如一台 AFFiNE 服务端）记对方的时钟、拉到的时钟、推过的本地时钟，都只往前走。`docs_to_push` 列出本地比推过的新的文档，`docs_to_pull` 列出对方比拉到的新的。
  - 附件按 `blobId` 存内容、MIME、大小，能标记删除、彻底删除；`blobs_to_upload` 列出还没传给对方的。
  - 接口是同步的（rusqlite），异步代码里用 `spawn_blocking`。表结构版本记在 SQLite 的 `user_version`，遇到更新的版本拒绝打开。
- 和 `architecture.md` 里“只用 PostgreSQL”的关系：这是 AFFiNE 内核给断网编辑用的本地文件，不放 TCMS 的数据，TCMS 不依赖它；`architecture.md` 那一条写明了这个例外。
- 测试：9 个，都用内存库或临时目录里的文件。更新合成快照；关掉再打开数据还在（重启）；断网时本地改两次、服务端别人也改，连上后按状态向量推差量、拉对方的改动，两边一样、谁的字都在（两端同时改）；时钟只往前走；整篇快照只在更新时才换；坏更新不写；附件和上传记录；删文档；拒绝更新的表结构。都是本地造的数据，没连 AFFiNE。
- 下一步（K4 第二个 PR）：把它接到 `affine-sync-client` 上，断网时写本地、连上后按 `docs_to_push` 补推、按服务端的时间戳拉，对着 Docker 里的 AFFiNE 试一次断网再连上。

### 5.8 K4 第二个 PR 做了什么（2026-11-25）

- `affine-sync-client` 里加了 `Replica`（`src/replica.rs`）：一个工作区的本地副本，库是 `affine-store`，连接是 `SyncConnection`。`affine-sync-client` 因此多依赖 `affine-store` 和 `yrs`，没有新的 crate。
  - `write`：本地改的只写进库，不管连没连着。`pending` 列出还没推上去的。
  - `sync_doc`：拿本地的状态向量从服务端取本地缺的那段，记进库；再拿服务端回的状态向量算出服务端缺的那段推上去。服务端还没有这篇就整篇推、`intent: create`。两边同时改了，Yjs 合起来谁的字都在。
  - `sync_all`：问服务端每篇文档最后改的时间（`space:load-doc-timestamps`），比拉到的新的、本地时钟比推过的新的，挨个 `sync_doc`。第一次连上、重连以后、收到“工作区有文档变了”时用。
  - `apply_event`：别人的修改（广播）直接记进库并记下拉到了；解不开的、重连、工作区有变化，告诉调用的一方接着同步哪一篇还是全部。
  - 时钟只在做成以后才往前走，推、拉到一半断了下次再做一遍。推上去以后服务端的时间戳变新，下一次 `sync_all` 会对这篇再取一次差量（空的），不直接当成拉到了，这样推和取之间别人改的也不会漏，代价是每推一篇多一次来回。
  - 一个库只放一个工作区，库里记着的工作区 id 和给的不一样就拒绝。
  - 库的操作是同步的，`Replica` 在异步函数里直接调（单篇文档的读写很快）；文档很大、很多时要换成 `spawn_blocking`，等 K5 的基准看。
- 测试：假服务端（内存里每篇文档一个 yrs 的 Doc）4 个：断网改两次推不上去、记着，服务端别人同时改，连上以后两边一样、只推一次；本地新建的文档用 `create` 推；关掉库再开，没推的还在、照样推上去，库是别的工作区的拒绝打开；广播直接进库、解不开的和重连告诉调用方再同步。
- `tests/affine.rs` 加了一个对着真 AFFiNE 的（Docker 里的 `ghcr.io/toeverything/affine:0.27.5-beta.3`，2026-11-25 跑过）：没连的时候根文档和一篇文档只写本地，连上以后都推上去，另一条连接读得到、文档列表里有；丢掉连接（断网）本地改第一条，同时另一条连接改第二条；关掉库再开、重新连上，`sync_all` 拉到别人的、推上自己的，两边 Markdown 一样；连着的时候别人再改，广播进库，再 `sync_all` 不推不拉。没在网页编辑器里打开看过。
- 还没做：附件的上传、下载（`blobs_to_upload` 还没接）；`tcms-affine` 的同步还用它自己的方式，没换成 `Replica`（要不要换另说）。下一步 K5 基准。

### 5.9 K5 做了什么（2026-11-25）

- `tcms bench affine-doc [--check]`（`crates/tcms-cli/src/bench/affine.rs`），不要数据库，CI 里带 `--check` 跑。数据现造：1 千个块的文档（小标题、正文带粗体代码链接、列表、编号、引用、代码块、待办轮着来）；在一段里一个字一个字打出来的 1 万条小更新；100 篇、每篇 100 个块的工作区。门槛和实测在 `architecture.md` 第 10 节。
- 量出来一个问题并改了：`affine-store` 读文档时用 yrs 的 `merge_updates_v1` 合更新，1 万条要约 500 毫秒。改成放进一个空的 Doc（不做垃圾回收）一条一个事务地应用、再整篇编码，约 7 毫秒，结果也从 161 KB 变成 44 KB（连着打的字合成一段）。有缺了前面的、放不进去的更新时退回 `merge_updates_v1`，加了一个先收到后一条的测试。全放进一个事务里应用要约 90 毫秒，所以是一条一个事务。
- 和 `y-octo` 0.4.1（MIT）比了一次（2026-11-25，开发机 4 核，release；一次性的对比程序，不进仓库，不加依赖），同一份数据：

  | 做什么 | yrs 0.28 | y-octo 0.4.1 |
  | --- | --- | --- |
  | 解开一篇 1 千个块的文档（371 KB） | 中位数 5.2 毫秒 | 6.8 毫秒 |
  | 解开再整篇编码回去 | 5.7 毫秒 | 7.7 毫秒 |
  | `merge_updates_v1` 合 1 万条小更新 | 475 毫秒 | 7.2 毫秒 |
  | 一条一条应用 1 万条更新 | 6.6 毫秒 | 8.5 毫秒 |
  | 按一半的状态向量取差量（161 KB 的更新） | 0.75 毫秒 | 0.24 毫秒 |
  | 从更新里算状态向量 | 0.66 毫秒 | 0.20 毫秒 |

  两边的结果互相读得进去、内容一样。只有 `merge_updates_v1` 差得多，绕开以后和 y-octo 持平；差量和状态向量 y-octo 快 3 倍，但都在 1 毫秒以内。所以第 7 节第 3 件建议照推荐：继续用 yrs，不换（等维护者定）。
- 第 5.8 节说的 `spawn_blocking`：1 万条没合的更新第一次读约 25 毫秒，一条更新写库约 50 到 70 微秒，`Replica` 先不改，照旧在异步函数里直接调。
- 甲（K1 到 K5）到这里做完了。没做的：附件的上传下载、`tcms-affine` 换成 `Replica`、Markdown 里的表格和图片导进 AFFiNE；乙要不要做等维护者定（第 6 节）。

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
3. **CRDT 用哪个**：推荐继续用 `yrs`（A1 已经在用，社区大）；K5 拿 `y-octo` 比一次，差得多再换。`y-octo` 是 AFFiNE 自己服务端用的，和 AFFiNE 的行为最接近。2026-11-25 比过（5.9 节）：只有 yrs 的 `merge_updates_v1` 慢得多，`affine-store` 已经绕开，别的差不多，建议不换。
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
