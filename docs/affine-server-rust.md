# 乙：兼容 AFFiNE 网页版的 Rust 服务端（方案草案）

> 2026-11-28 写，草案，**还没定要不要做**。维护者 2026-11-28 在卡片上选了“写乙的方案草案”，只写文档不写代码。
> 前情在 [`affine-kernel-rust.md`](affine-kernel-rust.md)：第 3 节的四种理解里，甲（不带界面的文档内核）K1 到 K5 已经做完（TCMS #24 到 #38）；乙是在甲上面再写一个能让 AFFiNE 原版前端连上的服务端，替掉企业版后端。
> 依据和甲一样是 `reference/affine` 固定的那个提交（AFFiNE 0.27.5，`3d25727`）。这次只看了 MIT 的部分：`packages/common/graphql`（前端的 GraphQL 操作定义）、`packages/common/nbstore`（前端的存储和云同步）、`packages/frontend/core/src/modules/cloud`（登录、服务端信息）。企业版的 `packages/backend` 没有读，也没有复制任何代码。

## 1. 结论先说

- **能做，但是是另一个产品的规模。** 第一版（网页版能登录、建工作区、多人实时写文档、传图片和附件、邀请成员）约 12 到 16 个 PR、两位工程师加 Claude 约 3 个月；评论、历史、公开分享、检索再加 1 到 2 个月。之后每次 AFFiNE 前端升级都要跟。
- **前端要的接口比看上去少。** 前端一共有 187 个 GraphQL 操作，但服务端在 `serverConfig.features` 里可以说自己没有 AI、付费、检索、OAuth、评论这些功能，前端就不显示、不调用。去掉 AI（39 个）、付费和许可证（24）、管理后台（26）、日历（9）、MCP 凭据（4）以后，第一版要做的约 35 到 45 个。同步协议是 socket.io 上的 14 个事件，A2 和甲的 K3、K4 已经从客户端那一侧实现过一遍（`affine-sync-client`），对着真 AFFiNE 测过。
- **最大的不确定在许可和条款，不在技术。** 只照 MIT 的客户端反推接口是可以的，但：AFFiNE 是否允许第三方服务端接它的前端、“AFFiNE”这个名字能不能用在我们的服务端上，要先问清楚。维护者如果能从 AFFiNE 拿到企业版服务端代码的书面重新授权，乙会便宜很多（第 7 节第 2 条）。
- **建议：先做 S0（一周左右）**：把许可和条款问清楚，搭好“AFFiNE 原版网页版连我们服务端”的端到端测试架子；S0 做完再决定做不做 S1 以后。

## 2. 前端要服务端做什么（从 MIT 的客户端代码数出来的）

### 2.1 HTTP

| 接口 | 前端在哪用 | 第一版 |
| --- | --- | --- |
| 网页版的静态文件（`index.html`、JS、CSS） | 自托管时网页版由服务端自己发，前端请求同源的接口 | 要。文件从 AFFiNE 的 MIT 前端构建出来（或从官方镜像里取出前端那部分），版本钉死 |
| `/info` | 服务端版本、类型 | 要 |
| `/graphql` | 第 2.3 节 | 要 |
| `/api/auth/preflight`、`/api/auth/sign-in`、`/api/auth/sign-out`、`/api/auth/session`、`/api/auth/methods` | 邮箱密码登录、会话 | 要 |
| `/api/auth/magic-link`、`/api/oauth/callback`、`/api/auth/open-app/sign-in` | 邮件登录、OAuth、桌面端打开 | 不要（说没有 OAuth；邮件登录看要不要配邮件） |
| `/api/workspaces/:id/docs/:docId` | 不连 socket 时整篇取文档（`doc-static`） | 要 |
| `/api/workspaces/:id/blobs/v1/:key`、`blob-manifest/v1`、`readable-blob-manifest/v1` | 附件的取、清单（`nbstore` 的 `cloud/blob.ts`） | 要（S3） |
| `/api/workspaces/:id/public-docs/...` | 公开分享的页面 | 不要（S6） |
| 头像、链接预览等 | 账号设置、书签块 | 头像要，链接预览不要 |

### 2.2 同步：socket.io 上的 14 个事件

`packages/common/nbstore/src/impls/cloud/socket.ts` 和 `doc.ts`、`awareness.ts` 里用到的：

| 事件 | 方向 | 第一版 |
| --- | --- | --- |
| `space:join-batch`、`space:leave-batch` | 前端发 | 要。带客户端版本、协议版本；加入的权限代数在新建文档后作废（A2 实测，`affine-sync.md` 7.2） |
| `space:load-doc`、`space:push-doc-update`、`space:load-doc-timestamps` | 前端发，要回执 | 要。和 `affine-sync-client` 用的一样 |
| `space:broadcast-doc-updates`、`space:broadcast-doc-invalidation` | 服务端推 | 要 |
| `space:delete-doc` | 前端发 | 要 |
| `space:load-workspace-state`、`space:doc-lifecycle` | 协议版本 2 才用 | 先不要：0.27.5 网页版报协议 1（A2 实测）。前端哪天默认用 2 再做 |
| `space:update-awareness`、`space:load-awarenesses`、`space:collect-awareness`、`space:broadcast-awareness-update` | 光标、在线的人 | 要（多人一起写时看得到别人） |

服务端这一侧要做的：按工作区和文档记加入的连接；收到推送合进存储、广播给同一篇的其他连接（不发回给自己，A2 实测）；按状态向量回差量（`affine-store` 的做法，合并用 K5 里改过的 `merge_updates`）；权限检查。

### 2.3 GraphQL：187 个操作按用途分

| 类 | 个数 | 第一版 |
| --- | --- | --- |
| 工作区和文档（`create-workspace`、`get-workspaces`、`get-workspace`、`get-workspace-page-meta`、`quota`、`server-config` 等） | 10 | 大多要 |
| 账号（当前用户、改名、头像、改密码、设置、验证邮箱等） | 20 | 约一半要 |
| 成员、邀请、权限（邀请链接、接受邀请、工作区和文档的角色、成员数、最近改过的文档） | 17 | 约 10 个要 |
| 附件（`blob-set`、`blob-list`、`blob-delete`、分片上传、`workspace-blob-quota`） | 9 | 要 |
| 评论（含回复、解决、附件、提到人） | 11 | 不要（S5；features 里不报 `Comment`） |
| 通知 | 3 | 不要（S5） |
| 历史（`histories`、`recover-doc`） | 2 | 不要（S5） |
| 公开分享 | 8 | 不要（S6） |
| 检索（`indexer-*`） | 3 | 不要（S6；不报 `Indexer`，前端用本地检索） |
| AI（copilot、自带 key 等） | 39 | 不做 |
| 付费和许可证 | 24 | 不做（不报 `Payment`） |
| 管理后台 | 26 | 不做，第一个账号在服务端命令行建 |
| 日历 | 9 | 不做 |
| MCP 凭据 | 4 | 不做（TCMS 自己就是 MCP 服务端） |
| 片段（被别的操作引用） | 2 | 跟着要 |

“第一版要”的约 35 到 45 个是按名字估的。S0 里在网页版上把第一版要的流程从头点一遍，记下真实发出的请求，再定准。

接口的类型照 `packages/common/graphql` 里的操作定义和 `schema.ts` 写。`schema.ts` 是从服务端的 GraphQL schema 生成的，放在 MIT 的目录里；我们只用它知道字段名和类型，自己写服务端的 schema（`async-graphql`），不复制它，也不读企业版里 schema 的源文件。

## 3. 架构

- 一个 Rust 进程：`axum`（HTTP、静态文件）+ `socketioxide`（socket.io 服务端，MIT）+ `async-graphql`（MIT/Apache-2.0）。和 TCMS 用的是同一套运行时（tokio）。
- 文档：用甲的 crate。存储照 `affine-store` 的思路（快照 + 后来的更新、每篇的时钟），但放在 PostgreSQL 里（多台机器、备份），和 I.P.、TCMS 共用一个库、单独一个 schema（暂叫 `affine`）。工作区的文档列表、文件夹用 `affine-doc` 的 `workspace` 模块读。
- 附件：本地磁盘和 S3 兼容存储，两种都做；元数据在 PostgreSQL。
- 账号：邮箱加密码（argon2），会话 cookie；第一个账号用命令行建。邮件（邀请、改密码）可选，配了 SMTP 才发。
- 权限：工作区的角色（所有者、管理员、成员、外部）和文档的角色，先照前端会调用的接口做，规则在 S4 写细。
- 测试：
  - `affine-sync-client` 现有的对真 AFFiNE 的测试，改成也能对着我们的服务端跑（同一套测试两边都过，最直接地说明协议对）；
  - 用无头浏览器跑 AFFiNE 原版网页版，连我们的服务端：登录、建工作区、写字、两个标签页互相看到、传图片、邀请第二个账号；
  - `tcms bench` 那样的基准：推一条更新到另一个连接收到广播的时间、1 万条更新的合并、100 个连接同时写一篇。

## 4. 分步

| 步 | 内容 | 验收 | 估计 |
| --- | --- | --- | --- |
| S0 | 问清许可和条款（第 7 节第 1、2 条）；决定放哪个仓库；搭端到端测试架子：钉一个 AFFiNE 版本的网页版静态文件，无头浏览器连一个服务端跑登录到写字的流程，先对着官方镜像跑通；在网页版上点一遍第一版的流程，记下真实请求的列表 | 架子对着官方 AFFiNE 跑通；第一版要做的接口列表定下来 | 1 到 2 个 PR，1 周 |
| S1 | 服务端外壳：静态文件、`/info`、`serverConfig`、邮箱密码登录、会话、当前用户；PostgreSQL 的表和迁移 | 网页版打开我们的服务端，能登录、看到空的工作区列表 | 2 到 3 个 PR，2 周 |
| S2 | 工作区和同步：建、列、删工作区；同步网关 14 个事件里的 12 个（不含协议 2 的两个）；文档存储；awareness | `affine-sync-client` 的测试对着我们的服务端全过；网页版里建文档、写字，两个标签页互相看到；重启服务端数据还在 | 3 到 4 个 PR，3 到 4 周 |
| S3 | 附件：磁盘和 S3、分片上传、清单、配额 | 网页版里插图片、附件，刷新以后还在；超过配额时网页版提示 | 2 个 PR，1.5 周 |
| S4 | 成员和权限：邀请链接、接受邀请、角色、离开工作区；可选的邮件 | 第二个账号接受邀请后能读写；只读成员写不进去（服务端拒绝） | 2 到 3 个 PR，2 周 |
| S5 | 评论、通知、文档历史和恢复 | 网页版里评论、解决、看历史、恢复到旧版本 | 2 到 3 个 PR，2 到 3 周 |
| S6 | 公开分享、服务端检索（tantivy）、从官方 AFFiNE 迁移数据（导出整个工作区再导入，用 K2 的导出包） | 分享链接不登录能看；检索结果和网页版本地检索一致；从官方 AFFiNE 迁过来的工作区打开正常 | 3 到 4 个 PR，3 到 4 周 |

S1 到 S4 是第一版（约 3 个月），S5、S6 再 1 到 2 个月。每次 AFFiNE 升级前端，跑一遍端到端测试，补上新用到的接口。

## 5. 许可要守的规矩（和甲一样，再多几条）

- 只看 MIT 的部分：前端（`packages/frontend`、`packages/common` 里除了 `native`）、`blocksuite`、`.gql` 操作定义、`schema.ts`。企业版的 `packages/backend`、`packages/common/native` 不读、不复制，没有书面重新授权前不变。
- 不依赖 AGPL 的 crate（`affine_doc_loader` 等，见 `affine-kernel-rust.md` 2.2）。
- 网页版的静态文件是 MIT，可以随服务端一起发，但要带上 AFFiNE 的版权和许可声明。
- 不用“AFFiNE”做我们服务端的名字或标志，说明里写“兼容 AFFiNE 网页版”；能不能这样写也在 S0 里问。

## 6. 风险

- **前端升级。** 同步协议和 GraphQL 都是 AFFiNE 客户端自己用的，没有承诺对外稳定（`affine-sync-client` 的文档里也写了）。每升一次版本都可能多出要做的接口或改字段。办法：钉版本，升级前跑端到端测试；只在测过的版本范围里报兼容。
- **前端对服务端的隐含假设。** 比如错误名（`DOC_NOT_FOUND`、`SPACE_ACCESS_DENIED`）、权限代数、时间戳的单位，只能从客户端代码和实测里找，读不到服务端的实现。A2 和 K3、K4 已经踩过几个（`affine-sync.md` 7.2），会更多。
- **工作量比估的大。** 上面按名字估的接口数可能偏少；S0 定了真实列表再重估一次。
- **和 TCMS 的关系。** 做成以后 TCMS 可以不再单独跑一个官方 AFFiNE，改连我们的服务端；也可能出现两边行为不一样（同一个文档在官方 AFFiNE 和我们服务端上表现不同）。

## 7. 要维护者定的事

1. **AFFiNE 是否同意第三方服务端接它的前端，名字怎么用。** 维护者和 AFFiNE 熟，建议直接问；有书面答复再进 S1。
2. **企业版服务端能不能重新授权。** 如果能拿到书面的、和 MIT 兼容的授权（全部或者同步网关、文档存储这几块），乙可以照着它写或直接搬，S1 到 S4 能省一半以上；拿不到就照第 5 节只看客户端。
3. **放哪个仓库。** 甲放在 TCMS 仓库里，因为它是 TCMS 的依赖。乙是一个独立的产品，推荐**新建一个仓库**（MIT），把甲的 `affine-doc`、`affine-sync-client`、`affine-store` 连历史一起拆过去，TCMS 改成依赖它们；新建仓库要你同意。
4. **第一版的范围。** 推荐第 4 节的 S1 到 S4（网页版能登录、多人写文档、附件、邀请）。要不要把评论（S5）提到第一版：TCMS 的提议现在是用 AFFiNE 的评论显示的（A4），如果 TCMS 要改连我们的服务端，评论就得在第一版里。
5. **数据库。** 推荐和 I.P.、TCMS 共用一个 PostgreSQL，单独一个 schema `affine`，和现在的做法一致。
