//! 条目用了多少次（迁移 0006，`docs/spec.md` 第 5 节“晋升”）：找晋升候选时看哪些岗位、团队、项目的条目用得多。

use serde::Serialize;
use sqlx::Row;

use crate::{Result, Store};

/// 只留最近多少天。
pub const KEEP_DAYS: i32 = 90;

/// 一条最近用了多少次。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ItemUse {
    pub item_id: String,
    /// 一共用了几次。
    pub uses: i64,
    /// 在几天里用过。
    pub days: i64,
    /// 哪些岗位用过，按名字排。
    pub roles: Vec<String>,
}

impl Store {
    /// 记下 `day`（`YYYY-MM-DD`，UTC）这一天每条被哪个岗位用了几次（加到已有的次数上），
    /// 顺便删掉 [`KEEP_DAYS`] 天以前的。`uses` 是（条目 id，岗位，次数）。
    pub async fn add_uses(&self, day: &str, uses: &[(String, String, i32)]) -> Result<()> {
        let uses: Vec<_> = uses.iter().filter(|(_, _, n)| *n > 0).collect();
        let mut tx = self.pool.begin().await?;
        if !uses.is_empty() {
            let ids: Vec<&str> = uses.iter().map(|(id, _, _)| id.as_str()).collect();
            let roles: Vec<&str> = uses.iter().map(|(_, r, _)| r.as_str()).collect();
            let counts: Vec<i32> = uses.iter().map(|(_, _, n)| *n).collect();
            sqlx::query(
                "INSERT INTO item_uses (item_id, day, role, uses) \
                 SELECT u.id, $1::date, u.role, sum(u.n)::integer \
                 FROM unnest($2::text[], $3::text[], $4::integer[]) AS u(id, role, n) \
                 GROUP BY u.id, u.role \
                 ON CONFLICT (item_id, day, role) DO UPDATE SET uses = item_uses.uses + EXCLUDED.uses",
            )
            .bind(day)
            .bind(&ids)
            .bind(&roles)
            .bind(&counts)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("DELETE FROM item_uses WHERE day < $1::date - $2::integer")
            .bind(day)
            .bind(KEEP_DAYS)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 到 `today`（`YYYY-MM-DD`，UTC）为止的最近 `days` 天（含今天）每条用了多少次，用得多的在前。
    pub async fn recent_uses(&self, today: &str, days: i32) -> Result<Vec<ItemUse>> {
        let rows = sqlx::query(
            "SELECT item_id, sum(uses)::bigint AS uses, count(DISTINCT day) AS days, \
                    array_agg(DISTINCT role ORDER BY role) AS roles \
             FROM item_uses WHERE day > $1::date - $2::integer AND day <= $1::date \
             GROUP BY item_id ORDER BY uses DESC, item_id",
        )
        .bind(today)
        .bind(days)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(ItemUse {
                    item_id: r.try_get("item_id")?,
                    uses: r.try_get("uses")?,
                    days: r.try_get("days")?,
                    roles: r.try_get("roles")?,
                })
            })
            .collect()
    }
}
