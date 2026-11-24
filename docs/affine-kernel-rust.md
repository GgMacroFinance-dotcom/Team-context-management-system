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
| 做什么 | Rust 库：块模型和 27 种块的属性、工作区根文档（文档列表、文件夹、文档属性）、Markdown 和 BlockSuite 快照的导入导出、同步协议客户端、本地 SQLite 存储 | 再写一个服务端：socket.io 同步网关、文档和附件存储、登录、工作区和成员、评论，接 AFFiNE 原版前端 | 用 Rust 写 `blocksuite/framework/store` 的替代品，编译成 WASM 给编辑器用 | 重写编辑器 |
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
| A2：登录、socket.io、取文档、推更新、收广播、断线重连、GraphQL 的工作区和评论 | 另一个线程正在做，放在 `crates/tcms-affine-client`，不引用 TCMS 别的 crate | K3 就是它。**不重复写**：K3 只是把它挪进内核的 crate。根文档的文档列表、文件夹、文档属性 A2 不写，用 `affine-doc` 的 |
| A3 到 A5：同步循环、审核页、基准 | 没做 | 留在 TCMS，用内核的库 |

为了不和 A2 撞车，建议：内核这边先做 K1、K2（只动文档模型和导入导出），等 A2 合并后再做 K3 的搬迁。A2 那边只需要注意一件事：连接层别引用 `tcms-core` 之类的 TCMS 类型，以后好搬。

## 5. 甲的分步

都先用本地造的文档和 AFFiNE 仓库里 MIT 的测试快照测，K3 起对着 Docker 里自托管的 AFFiNE 测，不花模型的钱。

| 步 | 内容 | 怎么测 | 估计 |
| --- | --- | --- | --- |
| K0 | 这份方案 | — | 本 PR |
| K1 | 新 crate `affine-doc`：从 `tcms-affine` 拆出不依赖 TCMS 的部分（读写块树、Markdown 子集），`tcms-affine` 改成调它；补齐 27 种块的结构（先只读，认得每种块的属性，不认得的原样留着）；工作区根文档（`meta.pages`、`folders`、`docProperties`）的读写 | A1 的 13 个测试照过；每种块各造一个文档往返；AFFiNE 的 MIT 测试快照能读 | 1 到 2 个 PR，1.5 周 |
| K2 | 导入导出：BlockSuite 快照（`.snapshot.json` 和导出的 zip）读写；Markdown 补上表格、图片引用、数据库块转成表格（只导出）；按工作区整体导出、导入 | 用 AFFiNE 自己导出的 zip 读进来再写出去，再导回 AFFiNE 看打不打得开（手动一次，记在 PR 里） | 2 个 PR，2 周 |
| K3 | 同步协议客户端 `affine-sync-client`：就是 A2 的连接层挪过来 | A2 自己的测试 | A2 合并后 1 个小 PR，2 天 |
| K4 | 本地存储：SQLite 里存更新、快照、时钟，定期合并，断网时能改、连上后补推（参考 MIT 的 `affine_nbstore` 的设计，不复制代码，不用它依赖的 AGPL crate） | 断网改、两端同时改、重启 | 1 到 2 个 PR，1.5 周 |
| K5 | 基准 `tcms bench affine-doc`：读一篇 1 千个块的文档、合并 1 万条更新、导出整个工作区；同一份数据和 `y-octo` 比一次 | 门槛写进 `architecture.md` 第 10 节 | 1 个 PR，3 天 |

### 5.1 K1 第一个 PR 做了什么（2026-11-24）

- 新 crate `crates/affine-doc`（MIT，只依赖 yrs、comrak、similar、regex、getrandom、thiserror）：A1 的 `doc`、`markdown`、`model`、`sections` 原样搬过来（用 `git mv`，历史还在）；`tcms-affine` 只剩一行 `pub use affine_doc::*`，以后 A3 的同步循环写在那里。手动对照用的环境变量改名为 `AFFINE_DOC_FIXTURE`、`AFFINE_DOC_DUMP`。
- `order`：AFFiNE 列表用的排序键。照公开的 `fractional-indexing` 算法自己写了一遍，外面再加 AFFiNE 的 `0` 加 32 个随机字符的后缀；生成的键能被 AFFiNE 再拿去当邻居。
- `workspace`：根文档 `meta` 的工作区名字和文档列表（读 BlockSuite 写的 Y.Map 形式，也读普通对象形式；认不得的字段原样留着）；工作区“数据库”的通用表（`db$<表名>`，删除标记 `$$DELETED`）；文件夹树（建文件夹、放链接、改名、挪动、连同下面一起删，规矩和 AFFiNE 前端一样：根上只放文件夹、不能挪进自己下面）；文档属性（`custom:*` 这类不认得的字段不丢）。
- 测试：原来的 13 个加上新的 11 个（排序键 6 个，其中 5 条是 AFFiNE 自己测试里写明的行为；工作区 5 个，都编码成 Yjs 更新在另一个 Doc 里应用再读）。都是自己造的数据，**还没拿真 AFFiNE 的根文档和 `db$folders` 对照过**：A2 那边对着 Docker 里的 AFFiNE 0.27.5-beta.3 核对过文件夹表的字段和排序键格式，和这里一致；拿真文档做的对照测试等 A2 合并后补。
- 还没做：27 种块的结构（K1 第二个 PR）；0.27.5 服务端的“工作区状态”（开了以后文档列表以服务端为准），等 A2 核对。

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
