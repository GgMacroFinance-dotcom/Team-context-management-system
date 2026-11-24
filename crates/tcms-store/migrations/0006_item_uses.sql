-- 条目用了多少次（docs/spec.md 第 5 节“晋升”）：agent 经 MCP 用到一条记一次——context_get 按任务挑中、放了全文，
-- 或者 context_read 读了它——按天（UTC）、按用的岗位合计。服务在内存里攒着，每半分钟左右写一次。
-- 只给审核页、命令行找晋升候选用，agent 读不到。只留最近 90 天。
CREATE TABLE item_uses (
    item_id text NOT NULL,
    day date NOT NULL,
    role text NOT NULL,
    uses integer NOT NULL CHECK (uses > 0),
    PRIMARY KEY (item_id, day, role)
);
CREATE INDEX item_uses_day ON item_uses (day);
