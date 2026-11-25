-- 提议在 AFFiNE 里显示成的评论（docs/affine-sync.md 5.5、7.6）：一条提议一条评论，发在它的目标条目（新建时是目标文件）
-- 所在的文档上。批准、拒绝以后回复结果、标成已解决（closed）。人在 AFFiNE 里的回复跟着同步时取回来，审核页里列出来。
-- 只有同步程序写，审核页读。
CREATE TABLE affine_comments (
    proposal_id     text PRIMARY KEY REFERENCES proposals (id) ON DELETE CASCADE,
    workspace_id    text NOT NULL,
    doc_id          text NOT NULL,
    -- 发评论时文档对应的文件。
    path            text NOT NULL,
    comment_id      text NOT NULL UNIQUE,
    -- 回复了结果、标成了已解决（或者评论在 AFFiNE 里被删了），之后不再管它。
    closed          boolean NOT NULL DEFAULT false,
    -- 人在 AFFiNE 里的回复：[{id, user, text, at}]。
    replies         jsonb NOT NULL DEFAULT '[]',
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX affine_comments_open ON affine_comments (doc_id) WHERE NOT closed;
