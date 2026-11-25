-- 和 AFFiNE 同步（docs/affine-sync.md 第 5 节）：上下文仓库里的文件、条目和 AFFiNE 里的文档、标题块怎么对上，
-- 上次同步完两边一致的样子（“底稿”，三方比较用），两边都改了、没自动合的条目。
-- 只有同步程序（tcms affine ...）用，MCP 不读这几张表。

-- 接上的范围：对应 AFFiNE 工作区根上的一个文件夹。
CREATE TABLE affine_scopes (
    scope           text PRIMARY KEY,
    workspace_id    text NOT NULL,
    -- 文件夹树（db$<工作区 id>$folders）里那个文件夹节点的 id。
    folder_id       text NOT NULL,
    -- 整个范围都接上（范围里新加的文件、文件夹里新建的文档也同步），还是只接了几个文件。
    whole           boolean NOT NULL,
    linked_at       timestamptz NOT NULL DEFAULT now()
);

-- 一个文件对应一篇文档。
CREATE TABLE affine_docs (
    path            text PRIMARY KEY,
    scope           text NOT NULL REFERENCES affine_scopes (scope) ON DELETE CASCADE,
    workspace_id    text NOT NULL,
    doc_id          text NOT NULL,
    -- 上次同步完这篇文档整篇的 Yjs 状态（v1 编码）。下次只取这以后的差量。
    state           bytea NOT NULL,
    -- 上次同步完这个文件在上下文仓库里的内容（blob id）和那时的提交；文件没变、文档也没变就跳过。
    git_blob        text,
    synced_commit   text,
    -- 停下来的原因（一次要归档一半以上的条目、AFFiNE 里移进了回收站、写完读回来对不上……）；为空是照常同步。
    paused          text,
    synced_at       timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, doc_id)
);

-- 一条对应一个二级标题块。base_title、base_body 是上次同步完两边一致的样子（按上下文仓库里的写法）。
CREATE TABLE affine_items (
    item_id         text PRIMARY KEY,
    path            text NOT NULL REFERENCES affine_docs (path) ON DELETE CASCADE,
    heading_id      text NOT NULL,
    base_title      text NOT NULL,
    base_body       text NOT NULL,
    UNIQUE (path, heading_id)
);

-- 没自动写过去、等人处理的条目。处理之前这一条两边都不动。
CREATE TABLE affine_conflicts (
    id              bigserial PRIMARY KEY,
    path            text NOT NULL,
    -- 上下文仓库里的条目 id；AFFiNE 里新加、还没写进 git 的一条没有。
    item_id         text UNIQUE,
    -- AFFiNE 里的二级标题块；git 里新加、还没写进 AFFiNE 的一条没有。
    heading_id      text,
    -- both：两边改得不一样；affine_deleted：AFFiNE 里删了、git 里改了；git_deleted：git 里删了、AFFiNE 里改了；
    -- readonly：git 里的写法 AFFiNE 表示不全（表格、图片……），在 AFFiNE 里改了不往回写；invalid：写不过去。
    reason          text NOT NULL CHECK (reason IN ('both', 'affine_deleted', 'git_deleted', 'readonly', 'invalid')),
    message         text,
    -- 为空表示这一边删掉了（或者还没有）。
    git_title       text,
    git_body        text,
    affine_title    text,
    affine_body     text,
    base_title      text,
    base_body       text,
    found_at        timestamptz NOT NULL DEFAULT now(),
    UNIQUE (path, heading_id)
);
