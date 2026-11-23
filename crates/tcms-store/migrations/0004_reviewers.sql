-- 审核的人的令牌（docs/architecture.md 第 5、7 节）：登录审核页，看、批准、拒绝提议，改单条。
-- 和 agent 的令牌放在同一张表，用 kind 分开：agent 的令牌连不了审核页，审核的人的令牌连不了 MCP。
-- 审核的人没有岗位；第一版能看、能处理所有范围。
ALTER TABLE tokens ADD COLUMN kind text NOT NULL DEFAULT 'agent' CHECK (kind IN ('agent', 'reviewer'));
ALTER TABLE tokens ALTER COLUMN role DROP NOT NULL;
ALTER TABLE tokens ADD CONSTRAINT tokens_role_by_kind CHECK ((kind = 'agent') = (role IS NOT NULL));
