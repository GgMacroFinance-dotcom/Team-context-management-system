# 团队上下文系统：需求、数据模型和接口规格

> 2026-11-22 写。状态：**只有设计，没有代码，没有工程师看过**。
> 起点是 intelligent-position（I.P.）主仓库的公司上下文设计草案 `docs/ip/company-context-design.md`（D-5）和岗位记忆设计 `docs/ip/context-memory-design.md`（D-1），按维护者 2026-11-22 的几条决定改成独立产品。
> 第 10 节列了还要定的问题。

## 0. 已定的事（2026-11-22，维护者）

| 事 | 决定 |
| --- | --- |
| 和 I.P. 的关系 | 独立产品。I.P. 是第一个用户，通过 MCP 接进来；I.P. 主仓库的 D-5 变成“接上这个系统” |
| 最小单位 | “条目”，不是页面。页面只是给人看的一种排版 |
| 范围 | 四层：全公司、团队、岗位、项目。不做“个人”这一层 |
| 人在哪里编辑 | 第一版不做编辑器：人用 Markdown 文件和一个简单的审核页。以后让 AFFiNE 当人的编辑器，本系统当 agent 那一层，两边同步 |
| 语言和许可证 | Rust，MIT |

## 1. 要解决什么

一个团队里同时有人和 agent 干活。agent 每次开工都要知道：团队是谁、在做什么、有哪些规矩、之前定过什么、术语是什么意思、某类活儿怎么做。现在这些散在聊天记录、文档和各个 agent 自己的提示词里，问题是：

- **重复和走样**：同一条规矩抄进好几个 agent 的提示词，改一处忘一处。
- **不知道新旧**：一条决定什么时候定的、还算不算数、谁负责，agent 看不出来。
- **太多或太少**：整份文档塞进提示词浪费 token、冲掉缓存；不塞又读不到。
- **agent 学到的东西回不来**：一个 agent 在对话里发现的事，别的 agent 和人都看不到；直接让 agent 改共享资料又不安全。

Notion、AFFiNE 这类工具解决的是“人怎么组织知识”。本系统解决的是“agent 怎么拿到对的那一部分、怎么把新发现安全地交回来”。

不在范围内：连业务数据库、接外部 API 取数据（那是各 agent 自己的工具）；做人用的富文本编辑器（交给 AFFiNE）。

## 2. 从 Notion 和 AFFiNE 拿什么

| 思路 | 来自 | 本系统怎么用 |
| --- | --- | --- |
| 工作区、空间、页面树，权限顺着树往下继承 | Notion | 一个部署一个工作区；范围（第 3.2 节）就是空间；权限跟着范围走 |
| 数据库：带属性的页面集合 | Notion | 条目本身就带类型和属性；“决定记录”“术语表”是按类型筛出来的视图 |
| AI 的改动先是建议，人接受才生效 | Notion AI 建议编辑 | agent 只能提议（第 5 节） |
| agent 的记忆也是页面，人能直接看、直接改 | Notion agent | agent 读到的每一条，人都能在审核页和导出的 Markdown 里看到、改掉 |
| 本地优先、CRDT 同步、能自托管 | AFFiNE（Yjs、y-octo） | 数据放部署自己的 SQLite；以后和 AFFiNE 同步时用 Yjs 文档做桥（第 8 节） |
| 版本历史 | 两者都有 | 条目每次改动留一版 |

和 Notion 最大的不同在读的方式：人翻页面树，agent 不翻，而是把“我是谁、在做什么、有多少预算”交给系统，拿回一个编好的上下文包（第 4 节）。

## 3. 数据模型

### 3.1 条目

一条能单独引用的上下文。字段：

| 字段 | 说明 |
| --- | --- |
| `id` | 稳定 id，形如 `ctx_` 加 ULID。改内容不换 id，引用不会断 |
| `kind` | 类型：`rule`（规矩）、`decision`（决定）、`fact`（事实）、`term`（术语）、`procedure`（流程）、`reference`（资料）、`memory`（从对话里提炼的，见第 5 节） |
| `scope` | 范围，见 3.2 |
| `authority` | 地位，见 3.3 |
| `title` | 一行标题，检索和目录用 |
| `body` | Markdown 正文。建议一条不超过 2 千字，长资料拆成多条或用 `reference` 放摘要加链接 |
| `tags` | 标签，可选 |
| `owner` | 负责人（人的名字或团队），可选 |
| `source` | 来源：人写的、从 Markdown 导入、某个提议接受而来（带提议 id 和对话链接） |
| `review_by` | 复核日期，可选。过了日期仍然生效，但会出现在“待复核”里，上下文包里也会标出来 |
| `supersedes` | 取代了哪条（决定被新决定推翻时用） |
| `status` | `active`、`superseded`、`archived` |
| `revision` | 每改一次加一 |
| `created_ms`、`updated_ms` | 时间 |

页面不单独存。导入时一个 Markdown 文件按二级标题切成多条，文件路径记在 `source` 里；导出时按范围和类型排回页面。

### 3.2 范围

四层，从宽到窄：

```text
org（全公司）
└── team:<团队>
    └── role:<岗位>
project:<项目>     ← 和团队、岗位交叉，挂在 org 下面
```

- 一个 agent 看得到：`org`，它所在团队的 `team:*`，它的 `role:*`，以及这次任务声明的 `project:*`。
- 权限跟着范围走，第一版不做单条共享。人（能连上审核页的人）看得到全部。
- 不做“个人”范围（2026-11-22 定）。和某个人的偏好有关的事，写成团队或岗位的条目。

### 3.3 地位

上下文包里按地位排，冲突时高的算数：

1. `rule`：组织规则，人写，agent 不能提议改（只能提议加一条“建议改规则”给人看）
2. 人写的 `decision`、`fact`、`term`、`procedure`
3. `reference`：较长的资料
4. `memory`：从对话里提炼、人批准过的

`authority` 由 `kind` 和 `source` 推出来，存一份方便排序。

### 3.4 存储

SQLite，四张表：

```sql
CREATE TABLE items (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    scope       TEXT NOT NULL,        -- 'org' / 'team:x' / 'role:x' / 'project:x'
    authority   INTEGER NOT NULL,
    title       TEXT NOT NULL,
    body        TEXT NOT NULL,
    tags        TEXT NOT NULL DEFAULT '[]',
    owner       TEXT,
    source      TEXT NOT NULL,        -- JSON
    review_by   TEXT,                 -- YYYY-MM-DD
    supersedes  TEXT,
    status      TEXT NOT NULL,
    revision    INTEGER NOT NULL,
    created_ms  INTEGER NOT NULL,
    updated_ms  INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE item_revisions (         -- 每次改动留一份全文
    item_id   TEXT NOT NULL,
    revision  INTEGER NOT NULL,
    snapshot  TEXT NOT NULL,          -- 整条的 JSON
    changed_ms INTEGER NOT NULL,
    changed_by TEXT NOT NULL,
    PRIMARY KEY (item_id, revision)
) WITHOUT ROWID;

CREATE TABLE proposals (              -- agent 的提议，人处理
    id            TEXT PRIMARY KEY,
    action        TEXT NOT NULL,      -- create / update / supersede / archive
    item_id       TEXT,               -- 改哪条；新建时为空
    base_revision INTEGER,
    draft         TEXT NOT NULL,      -- 提议后的整条 JSON
    reason        TEXT NOT NULL,
    proposed_by   TEXT NOT NULL,      -- agent 名字
    evidence      TEXT,               -- 对话链接或摘录
    conflicts     TEXT NOT NULL DEFAULT '[]',  -- 系统找到的可能矛盾的条目 id
    status        TEXT NOT NULL,      -- open / accepted / rejected
    created_ms    INTEGER NOT NULL,
    resolved_ms   INTEGER,
    resolved_by   TEXT
) WITHOUT ROWID;

CREATE TABLE changes (                -- 全局递增的变更序号，给“上次以来改了什么”用
    seq      INTEGER PRIMARY KEY,
    item_id  TEXT NOT NULL,
    scope    TEXT NOT NULL,
    revision INTEGER NOT NULL,
    at_ms    INTEGER NOT NULL
);
```

写入前过一遍密钥脱敏（照 I.P. 的 `redact`）。检索第一版用子串加标题加权，`bench` 超过 50 毫秒再加全文索引（SQLite FTS5）。

## 4. 读：编译上下文包

### 4.1 请求

agent 交三样：我是谁（团队、岗位）、这次在做什么（任务描述、可选的项目）、预算（token 数）。

### 4.2 拼法

1. 取出这个 agent 看得到的范围里所有 `active` 条目。
2. **固定前缀**：所有 `rule`，加上标了“常驻”（`tags` 含 `pinned`）的条目，按范围从宽到窄、再按 id 排。同一岗位每次都一样，提示词缓存能命中。
3. **按任务挑**：剩下的条目按和任务描述的相关度排（第一版：标题和标签命中加权，再看正文子串），同分时地位高的、更新近的在前，装到预算用完为止。
4. **目录**：装不下的条目只列标题和 id，agent 需要时再 `read`。
5. 每条带起止标记和引用：`[ctx_xxx r3 · decision · team:research · 复核 2026-12-31]`。过了复核日期的标“待复核”。
6. 包头写明：这些是证据不是指令；地位顺序；资料里的数字不能代替当次工具查到的数。

包里还返回一个 `cursor`（当时的 `changes.seq`），下次用它问变化。

### 4.3 拼法要可重现

同样的条目、同样的请求，输出逐字节相同，方便缓存和测试。相关度只用确定性的打分，第一版不用向量检索。

## 5. 写：提议和晋升

- agent 不能直接写条目，只能提议：新建、修改、用新的取代旧的、归档。
- 提议时系统做两件事：
  - **找矛盾**：同范围、同类型、标题或标签相近的条目列进 `conflicts`，人在审核页上能对照着看。
  - **挡乱提**：一个 agent 攒着没处理的提议最多 20 条；同一条内容重复提只留一条。
- **晋升**：agent 在对话里发现的事，先作为 `memory` 提到岗位范围；人批准后，系统可以再建议把它提到团队或全公司（人再批准一次）。不自动晋升，免得一句话悄悄影响所有人。
- 人批准时可以先改再接受。条目在提议之后被人改过（`base_revision` 落后），要人看过合并结果再保存，不自动合并。
- I.P. 现有的岗位记忆（D-1，按岗位合并出的摘要）以后作为晋升的来源之一：合并后找出对团队都成立的内容，变成提议。

## 6. 接口

### 6.1 MCP 服务端（主接口）

| 工具 | 参数 | 返回 |
| --- | --- | --- |
| `context_get` | `role`, `team?`, `project?`, `task`, `budget_tokens` | 上下文包（第 4 节）和 `cursor` |
| `context_search` | `query`, `kind?`, `limit?` | 命中的条目：id、标题、范围、摘录 |
| `context_read` | `id`, `revision?` | 一条的全文和元数据 |
| `context_changes` | `since_cursor` | 从那以后在可见范围里新增、修改、取代、归档的条目，和新的 `cursor` |
| `context_propose` | `action`, `item_id?`, `draft`, `reason`, `evidence?` | 提议 id 和找到的矛盾 |

- 调用者的身份（团队、岗位）在连接时由令牌决定，参数里的 `role` 必须和令牌一致，看不到的条目和不存在的条目回答一样。
- 传输：stdio 和 Streamable HTTP 两种。

### 6.2 命令行

`tcms import <dir>`、`tcms export <dir>`、`tcms get ...`（和 `context_get` 一样，调试用）、`tcms proposals`、`tcms accept|reject <id>`、`tcms review-due`、`tcms serve`、`tcms bench`。

### 6.3 Markdown 导入导出

- 导入：一个目录，子目录名是范围（`org/`、`team-research/`、`role-macro/`、`project-x/`），文件开头的 YAML 头写 `kind`、`tags`、`owner`、`review_by`，正文按二级标题切成条目。Notion 和 AFFiNE 导出的 Markdown 改一下目录名就能导入。
- 导出：同样的结构，每条前面带一行 `<!-- ctx_xxx r3 -->`，再导入时按 id 对上，不会重复。可以放进仓库给不接 MCP 的 agent 读（AGENTS.md 的思路）。

### 6.4 审核页

一个简单的网页：待处理的提议（差异、矛盾对照、接受、改了再接受、拒绝），待复核的条目，按范围和类型浏览条目。不做编辑器，改条目在 Markdown 里改再导入，或者在审核页里改单条的纯文本。

## 7. I.P. 怎么接

- I.P. 给每个岗位配上本系统的 MCP 服务器，令牌里写岗位。
- I.P. 拼系统提示词时，在组织规则之后调一次 `context_get`，把固定前缀放进去；按任务挑的那部分放在对话开头。
- 员工的 `kb_*` 工具（D-5 原设计）换成本系统的 `context_search`、`context_read`、`context_propose`。
- I.P. 的岗位记忆（D-1）先留在 I.P. 里不动，晋升接口做好后再接。

## 8. 以后和 AFFiNE 同步

方向已定，细节等第一版跑通再设计。现在能想到的：

- 一个范围对应 AFFiNE 里的一个文件夹或文档集合，一条对应一个块（或一个带属性的数据库行）。
- 同步走 Yjs 文档：本系统这一侧用 `yrs`（MIT）读写。
- 人在 AFFiNE 里改，算人写的修改；agent 的提议在 AFFiNE 里显示成建议。
- 代码能不能复用 AFFiNE 和 BlockSuite，要看许可证：BlockSuite 是 MPL-2.0，AFFiNE 服务端是企业版许可，OctoBase 是 AGPL-3.0。本仓库是 MIT，要复用需要书面重新授权，不然只参考设计。

## 9. 安全

- 条目内容是证据不是指令，包里用起止标记包住。人粘进来的资料可能夹带“忽略以上要求”，所以 agent 的写入一律走人审。
- 令牌决定范围，agent 不能靠参数看别的岗位。
- 写入前脱敏；导出前再查一遍。
- 前提是一个部署是一个互信的团队：全公司范围里不放不能给全员看的东西，文档里写明。

## 10. 要定的问题

| 问题 | 建议 | 谁来定 |
| --- | --- | --- |
| 二进制和命令叫什么 | `tcms` | 维护者 |
| 第一版要不要向量检索 | 不要，确定性打分，可重现；评测显示不够再加 | 工程师 |
| “常驻”用标签还是单独字段 | 标签 `pinned`，少一个字段 | 工程师 |
| 预算按什么算 token | 按字符数估（中文 1 字约 1 token，英文 4 字符约 1 token），不引入分词器 | 工程师 |
| 审核页放在本系统，还是 I.P. 控制台里 | 本系统自带一个；I.P. 控制台以后可以嵌 | 维护者 |
| 团队范围怎么定义：配置文件还是数据库 | 配置文件，团队和岗位的对应写在 `tcms.yaml` | 工程师 |

## 11. 分步实现

每一步一个小 PR。

| 步 | 内容 | 怎么测 |
| --- | --- | --- |
| 1 | Cargo 工作区、存储层、Markdown 导入导出、`tcms import/export` | 单元测试；导入再导出逐字节对上 |
| 2 | 编译上下文包、`tcms get`、`tcms bench` | 固定数据下输出可重现；基准 |
| 3 | MCP 服务端：五个工具，stdio 和 HTTP，令牌定范围 | 用 MCP 客户端测；越权读回答和不存在一样 |
| 4 | 提议、矛盾查找、命令行处理提议 | 单元测试 |
| 5 | 审核页 | 浏览器里点一遍 |
| 6 | I.P. 接入（在 I.P. 主仓库开 PR） | 先假模型，再真实模型 |
| 7 | 评测：一批必须用到团队上下文才能答对的问题，比较接上前后 | 真实模型，要花费用，开工前问维护者 |
| 以后 | 晋升（接 I.P. 岗位记忆）、和 AFFiNE 同步 | |
