-- 提议（docs/spec.md 第 5 节、docs/architecture.md 第 3 节）：agent 不能直接写条目，只能提议，人批准后才写进 git。
-- 存的是提议里那一条完整的样子（修改时没写的字段从当时的条目补上），批准时照着写。
CREATE TABLE proposals (
    id              text PRIMARY KEY,
    action          text NOT NULL CHECK (action IN ('create', 'update', 'supersede', 'archive')),
    -- 修改、取代、归档的是哪一条，和提议时它的版本号（批准时比对，落后了要人看过再接受）。
    item_id         text,
    base_revision   integer,
    scope           text NOT NULL,
    kind            text NOT NULL,
    title           text NOT NULL,
    body            text NOT NULL,
    tags            text[] NOT NULL DEFAULT '{}',
    review_by       text,
    reason          text NOT NULL,
    evidence        text[] NOT NULL DEFAULT '{}',
    -- 提议时找到的相近条目：[{id, revision, title, scope, kind, similarity}]。
    conflicts       jsonb NOT NULL DEFAULT '[]',
    -- 同样的内容只留一条没处理的。
    content_hash    text NOT NULL,
    -- 脱敏抹掉了几处。
    redacted        integer NOT NULL DEFAULT 0,
    proposer_token  text NOT NULL,
    proposer_name   text NOT NULL,
    proposer_role   text NOT NULL,
    status          text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'accepted', 'rejected')),
    created_at      timestamptz NOT NULL DEFAULT now(),
    decided_at      timestamptz,
    decided_by      text,
    decision_note   text,
    -- 接受以后：写成了哪一条、哪个提交。
    result_item     text,
    result_commit   text
);

CREATE UNIQUE INDEX proposals_open_content ON proposals (content_hash) WHERE status = 'open';
CREATE INDEX proposals_open_by_token ON proposals (proposer_token) WHERE status = 'open';
CREATE INDEX proposals_by_status ON proposals (status, created_at);
