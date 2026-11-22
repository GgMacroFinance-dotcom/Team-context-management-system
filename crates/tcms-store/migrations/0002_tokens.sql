-- 令牌：agent 连 MCP 服务端用（docs/architecture.md 第 5、7 节）。库里只存哈希，明文只在建的时候显示一次。
-- 令牌绑定岗位、团队和能声明的项目，调用者能看哪些范围由令牌决定。
CREATE TABLE tokens (
    id          text PRIMARY KEY,
    name        text NOT NULL,
    token_hash  text NOT NULL UNIQUE,
    role        text NOT NULL,
    team        text,
    -- 能声明哪些项目；'{*}' 表示全部。
    projects    text[] NOT NULL DEFAULT '{}',
    created_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz,
    revoked_at  timestamptz
);

-- 审计：谁在什么时候做了什么（令牌增删，以后还有提议、批准、拒绝）。
CREATE TABLE audit (
    seq     bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    at      timestamptz NOT NULL DEFAULT now(),
    actor   text NOT NULL,
    action  text NOT NULL,
    target  text,
    detail  jsonb NOT NULL DEFAULT '{}'
);
