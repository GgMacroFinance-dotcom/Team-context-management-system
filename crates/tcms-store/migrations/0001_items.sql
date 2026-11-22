-- 条目索引。真相在上下文仓库（git）里，这里是从 git 建出来的，用来查询和排序。
-- 表名不带 schema，由连接的 search_path 决定（默认 tcms），测试和基准用各自的 schema。

CREATE TABLE items (
    id           text PRIMARY KEY,
    scope        text NOT NULL,
    kind         text NOT NULL,
    authority    smallint NOT NULL,
    title        text NOT NULL,
    body         text NOT NULL,
    tags         text[] NOT NULL DEFAULT '{}',
    owner        text,
    review_by    date,
    supersedes   text,
    status       text NOT NULL CHECK (status IN ('active', 'superseded', 'archived')),
    revision     integer NOT NULL CHECK (revision >= 1),
    content_hash text NOT NULL,
    path         text NOT NULL,
    commit_id    text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX items_scope_status_idx ON items (scope, status);
CREATE INDEX items_supersedes_idx ON items (supersedes) WHERE supersedes IS NOT NULL;

-- 每个版本一行：哪个提交、内容哈希、当时整条的快照（context_read 取旧版本用）。
CREATE TABLE item_versions (
    item_id      text NOT NULL,
    revision     integer NOT NULL,
    commit_id    text NOT NULL,
    content_hash text NOT NULL,
    snapshot     jsonb NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (item_id, revision)
);

-- 全局递增的变更序号，给“从上次到现在改了什么”用。
CREATE TABLE changes (
    seq       bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    item_id   text NOT NULL,
    scope     text NOT NULL,
    revision  integer NOT NULL,
    change    text NOT NULL CHECK (change IN ('created', 'updated', 'superseded', 'restored', 'archived')),
    commit_id text NOT NULL,
    at        timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX changes_scope_seq_idx ON changes (scope, seq);

-- 索引到了哪个提交。只有一行。
CREATE TABLE index_state (
    singleton  boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    commit_id  text NOT NULL,
    indexed_at timestamptz NOT NULL DEFAULT now()
);
