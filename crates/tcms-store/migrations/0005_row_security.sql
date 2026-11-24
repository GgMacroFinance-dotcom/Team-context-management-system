-- 行级安全（docs/architecture.md 第 5 节）：agent 读条目、旧版本、变更时，数据库按令牌能看的范围再拦一道。
-- 服务本身也按范围过滤，这一层是兜底：代码里哪里漏了，越权的行在数据库这里就出不来。
--
-- agent 的读在一个只读事务里，开头写上令牌能看的范围：
--   tcms.scopes        数组写法，比如 {org,team:research,role:macro,project:q4}
--   tcms.all_projects  on 表示任何项目都能看（令牌的项目是 *）
-- 没写 tcms.scopes 时不拦：服务自己建索引、审核页、命令行要看全部。写了就只放行这些范围，空数组什么都看不到。
--
-- 表的主人（服务的账号）也要受策略管，所以用 FORCE。超级用户和带 BYPASSRLS 的账号不受行级安全管，
-- 服务启动时查到会写一条提醒（服务本身的过滤照样在）。

CREATE FUNCTION visible_scope(scope text) RETURNS boolean
    LANGUAGE sql STABLE
AS $$
    SELECT CASE
        WHEN coalesce(current_setting('tcms.scopes', true), '') = '' THEN true
        ELSE coalesce(
            scope = ANY (current_setting('tcms.scopes', true)::text[])
                OR (scope LIKE 'project:%' AND current_setting('tcms.all_projects', true) = 'on'),
            false)
    END
$$;

ALTER TABLE items ENABLE ROW LEVEL SECURITY;
ALTER TABLE items FORCE ROW LEVEL SECURITY;
CREATE POLICY reach ON items USING (visible_scope(scope));

-- 旧版本按那个版本当时所在的范围（快照里的 scope）。
ALTER TABLE item_versions ENABLE ROW LEVEL SECURITY;
ALTER TABLE item_versions FORCE ROW LEVEL SECURITY;
CREATE POLICY reach ON item_versions USING (visible_scope(snapshot ->> 'scope'));

ALTER TABLE changes ENABLE ROW LEVEL SECURITY;
ALTER TABLE changes FORCE ROW LEVEL SECURITY;
CREATE POLICY reach ON changes USING (visible_scope(scope));
