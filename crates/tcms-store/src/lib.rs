//! PostgreSQL 里的条目索引（`docs/architecture.md` 第 5 节）。
//!
//! 只用标准 PostgreSQL，不要求扩展。所有表放在一个 schema 里（默认 `tcms`），
//! 连接时设 `search_path`，迁移文件里不写 schema 名，测试和基准可以用各自的 schema。

use std::str::FromStr;
use std::time::Duration;

use serde::Serialize;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, PgPool, Postgres, Row, Transaction};

/// 有变更时发的通知频道。频道是库级的，所以带 `tcms_` 前缀（和 I.P. 共用一个库）。
pub const NOTIFY_CHANNEL: &str = "tcms_changes";

/// 一次写入的行数上限。
const CHUNK: usize = 2000;

/// 取整条的 SELECT，后面接条件。写成宏是为了拼出来的仍是字面量。
macro_rules! select_row {
    ($tail:literal) => {
        concat!(
            "SELECT id, scope, kind, authority, title, body, tags, owner, review_by::text, supersedes, ",
            "status, revision, content_hash, path, commit_id FROM items ",
            $tail
        )
    };
}

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("数据库：{0}")]
    Db(#[from] sqlx::Error),
    #[error("数据库迁移：{0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("schema 名不对：{0}（只能是小写字母、数字、下划线，字母或下划线开头）")]
    BadSchema(String),
    #[error("索引期间有别的进程先更新了索引（原来在 {expected:?}，现在在 {found:?}），请重试")]
    Conflict {
        expected: Option<String>,
        found: Option<String>,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// 索引表里一条的完整内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ItemRow {
    pub id: String,
    pub scope: String,
    pub kind: String,
    pub authority: i16,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub owner: Option<String>,
    pub review_by: Option<String>,
    pub supersedes: Option<String>,
    pub status: String,
    pub revision: i32,
    pub content_hash: String,
    pub path: String,
    pub commit_id: String,
}

/// 做索引计划要用到的字段，不带正文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemState {
    pub id: String,
    pub status: String,
    pub revision: i32,
    pub content_hash: String,
    pub path: String,
    pub supersedes: Option<String>,
}

/// 写一个新版本：整条内容，加上这次变更的类型。
#[derive(Debug, Clone)]
pub struct ItemWrite {
    pub row: ItemRow,
    pub change: ChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Created,
    Updated,
    Superseded,
    Restored,
    Archived,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Created => "created",
            ChangeKind::Updated => "updated",
            ChangeKind::Superseded => "superseded",
            ChangeKind::Restored => "restored",
            ChangeKind::Archived => "archived",
        }
    }
}

/// `changes` 表的一行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangeRow {
    pub seq: i64,
    pub item_id: String,
    pub scope: String,
    pub revision: i32,
    pub change: String,
    pub commit_id: String,
}

/// 某一时刻所有生效（`active`）的条目，和当时的变更序号、索引到的提交。编上下文包用。
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// 按 id 排。
    pub items: Vec<ActiveItem>,
    /// 当时最大的变更序号，没有变更时是 0。
    pub cursor: i64,
    pub commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveItem {
    pub row: ItemRow,
    /// 索引里最后一次改这一条的时间（毫秒）。同分时更新近的排前面。
    pub updated_ms: i64,
}

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
    schema: String,
}

fn valid_schema(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && s.len() <= 63
}

impl Store {
    /// 连上数据库，没有 schema 就建，跑迁移。
    pub async fn connect(url: &str, schema: &str) -> Result<Self> {
        if !valid_schema(schema) {
            return Err(Error::BadSchema(schema.to_owned()));
        }
        let base = PgConnectOptions::from_str(url)?;
        // 先用不带 search_path 的连接建 schema（打包服务器上由初始化脚本建好，这里是兜底）。
        {
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .connect_with(base.clone())
                .await?;
            // schema 名在上面校验过，只含小写字母、数字、下划线。
            sqlx::query(AssertSqlSafe(format!(
                "CREATE SCHEMA IF NOT EXISTS {schema}"
            )))
            .execute(&pool)
            .await?;
            pool.close().await;
        }
        let options = base.options([("search_path", schema)]);
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self {
            pool,
            schema: schema.to_owned(),
        })
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// 删掉整个 schema（测试和基准收尾用）。
    pub async fn drop_schema(self) -> Result<()> {
        sqlx::query(AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {} CASCADE",
            self.schema
        )))
        .execute(&self.pool)
        .await?;
        self.pool.close().await;
        Ok(())
    }

    /// 索引到了哪个提交。
    pub async fn indexed_commit(&self) -> Result<Option<String>> {
        let row = sqlx::query("SELECT commit_id FROM index_state")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get(0)))
    }

    /// 开始一次索引：开事务，拿到这个 schema 的索引锁，确认索引还停在 `expected_base`。
    pub async fn begin_index(&self, expected_base: Option<&str>) -> Result<IndexTx> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended('tcms_index:' || current_schema(), 0))",
        )
        .execute(&mut *tx)
        .await?;
        let found: Option<String> = sqlx::query("SELECT commit_id FROM index_state")
            .fetch_optional(&mut *tx)
            .await?
            .map(|r| r.get(0));
        if found.as_deref() != expected_base {
            return Err(Error::Conflict {
                expected: expected_base.map(str::to_owned),
                found,
            });
        }
        Ok(IndexTx {
            tx,
            schema: self.schema.clone(),
            max_seq: None,
        })
    }

    pub async fn get_item(&self, id: &str) -> Result<Option<ItemRow>> {
        let row = sqlx::query(select_row!("WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| item_row(&r)))
    }

    /// 按 id 排序的全部条目（测试和调试用）。
    pub async fn all_items(&self) -> Result<Vec<ItemRow>> {
        let rows = sqlx::query(select_row!("ORDER BY id"))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(item_row).collect())
    }

    /// 生效条目、变更序号和索引提交，在同一个只读的可重复读事务里取，三样对得上。
    pub async fn snapshot(&self) -> Result<Snapshot> {
        let mut tx = self
            .pool
            .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .await?;
        let rows = sqlx::query(concat!(
            "SELECT id, scope, kind, authority, title, body, tags, owner, review_by::text, supersedes, ",
            "status, revision, content_hash, path, commit_id, ",
            "floor(extract(epoch FROM updated_at) * 1000)::int8 ",
            "FROM items WHERE status = 'active' ORDER BY id"
        ))
        .fetch_all(&mut *tx)
        .await?;
        let cursor: i64 = sqlx::query("SELECT coalesce(max(seq), 0)::int8 FROM changes")
            .fetch_one(&mut *tx)
            .await?
            .get(0);
        let commit: Option<String> = sqlx::query("SELECT commit_id FROM index_state")
            .fetch_optional(&mut *tx)
            .await?
            .map(|r| r.get(0));
        tx.commit().await?;
        Ok(Snapshot {
            items: rows
                .iter()
                .map(|r| ActiveItem {
                    row: item_row(r),
                    updated_ms: r.get(15),
                })
                .collect(),
            cursor,
            commit,
        })
    }

    /// 序号大于 `after` 的变更，按序号排。
    pub async fn changes_since(&self, after: i64) -> Result<Vec<ChangeRow>> {
        let rows = sqlx::query(
            "SELECT seq, item_id, scope, revision, change, commit_id FROM changes WHERE seq > $1 ORDER BY seq",
        )
        .bind(after)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| ChangeRow {
                seq: r.get(0),
                item_id: r.get(1),
                scope: r.get(2),
                revision: r.get(3),
                change: r.get(4),
                commit_id: r.get(5),
            })
            .collect())
    }

    /// 某个版本的快照。
    pub async fn version_snapshot(
        &self,
        id: &str,
        revision: i32,
    ) -> Result<Option<serde_json::Value>> {
        let row =
            sqlx::query("SELECT snapshot FROM item_versions WHERE item_id = $1 AND revision = $2")
                .bind(id)
                .bind(revision)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| r.get(0)))
    }
}

fn item_row(r: &sqlx::postgres::PgRow) -> ItemRow {
    ItemRow {
        id: r.get(0),
        scope: r.get(1),
        kind: r.get(2),
        authority: r.get(3),
        title: r.get(4),
        body: r.get(5),
        tags: r.get(6),
        owner: r.get(7),
        review_by: r.get(8),
        supersedes: r.get(9),
        status: r.get(10),
        revision: r.get(11),
        content_hash: r.get(12),
        path: r.get(13),
        commit_id: r.get(14),
    }
}

/// 一次索引的事务。`finish` 才提交；中途出错丢掉就会回滚。
pub struct IndexTx {
    tx: Transaction<'static, Postgres>,
    schema: String,
    max_seq: Option<i64>,
}

impl IndexTx {
    /// 所有条目的状态（不带正文）。
    pub async fn states(&mut self) -> Result<Vec<ItemState>> {
        let rows = sqlx::query(
            "SELECT id, status, revision, content_hash, path, supersedes FROM items ORDER BY id",
        )
        .fetch_all(&mut *self.tx)
        .await?;
        Ok(rows
            .iter()
            .map(|r| ItemState {
                id: r.get(0),
                status: r.get(1),
                revision: r.get(2),
                content_hash: r.get(3),
                path: r.get(4),
                supersedes: r.get(5),
            })
            .collect())
    }

    /// 取几条的完整内容。
    pub async fn rows(&mut self, ids: &[String]) -> Result<Vec<ItemRow>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(select_row!("WHERE id = ANY($1) ORDER BY id"))
            .bind(ids)
            .fetch_all(&mut *self.tx)
            .await?;
        Ok(rows.iter().map(item_row).collect())
    }

    /// 写入一批新版本：更新 `items`，记 `item_versions` 和 `changes`。按给定顺序记变更序号。
    pub async fn write(&mut self, writes: &[ItemWrite]) -> Result<()> {
        for chunk in writes.chunks(CHUNK) {
            self.write_chunk(chunk).await?;
        }
        Ok(())
    }

    async fn write_chunk(&mut self, writes: &[ItemWrite]) -> Result<()> {
        if writes.is_empty() {
            return Ok(());
        }
        let col =
            |f: fn(&ItemRow) -> String| writes.iter().map(|w| f(&w.row)).collect::<Vec<String>>();
        let opt = |f: fn(&ItemRow) -> Option<String>| {
            writes.iter().map(|w| f(&w.row)).collect::<Vec<_>>()
        };
        let ids = col(|r| r.id.clone());
        let scopes = col(|r| r.scope.clone());
        let revisions: Vec<i32> = writes.iter().map(|w| w.row.revision).collect();
        let hashes = col(|r| r.content_hash.clone());
        let commits = col(|r| r.commit_id.clone());
        let tags = col(|r| serde_json::to_string(&r.tags).expect("字符串列表总能序列化"));
        let snapshots = col(|r| serde_json::to_string(r).expect("条目总能序列化"));

        sqlx::query(
            "INSERT INTO items (id, scope, kind, authority, title, body, tags, owner, review_by, supersedes, \
                                status, revision, content_hash, path, commit_id)
             SELECT u.id, u.scope, u.kind, u.authority, u.title, u.body,
                    ARRAY(SELECT jsonb_array_elements_text(u.tags::jsonb)),
                    u.owner, u.review_by::date, u.supersedes, u.status, u.revision, u.content_hash, u.path, u.commit_id
             FROM UNNEST($1::text[], $2::text[], $3::text[], $4::int2[], $5::text[], $6::text[], $7::text[],
                         $8::text[], $9::text[], $10::text[], $11::text[], $12::int4[], $13::text[], $14::text[],
                         $15::text[])
                  AS u(id, scope, kind, authority, title, body, tags, owner, review_by, supersedes,
                       status, revision, content_hash, path, commit_id)
             ON CONFLICT (id) DO UPDATE SET
                 scope = EXCLUDED.scope, kind = EXCLUDED.kind, authority = EXCLUDED.authority,
                 title = EXCLUDED.title, body = EXCLUDED.body, tags = EXCLUDED.tags, owner = EXCLUDED.owner,
                 review_by = EXCLUDED.review_by, supersedes = EXCLUDED.supersedes, status = EXCLUDED.status,
                 revision = EXCLUDED.revision, content_hash = EXCLUDED.content_hash, path = EXCLUDED.path,
                 commit_id = EXCLUDED.commit_id, updated_at = now()",
        )
        .bind(&ids)
        .bind(&scopes)
        .bind(col(|r| r.kind.clone()))
        .bind(writes.iter().map(|w| w.row.authority).collect::<Vec<i16>>())
        .bind(col(|r| r.title.clone()))
        .bind(col(|r| r.body.clone()))
        .bind(&tags)
        .bind(opt(|r| r.owner.clone()))
        .bind(opt(|r| r.review_by.clone()))
        .bind(opt(|r| r.supersedes.clone()))
        .bind(col(|r| r.status.clone()))
        .bind(&revisions)
        .bind(&hashes)
        .bind(col(|r| r.path.clone()))
        .bind(&commits)
        .execute(&mut *self.tx)
        .await?;

        sqlx::query(
            "INSERT INTO item_versions (item_id, revision, commit_id, content_hash, snapshot)
             SELECT u.id, u.revision, u.commit_id, u.content_hash, u.snapshot::jsonb
             FROM UNNEST($1::text[], $2::int4[], $3::text[], $4::text[], $5::text[])
                  AS u(id, revision, commit_id, content_hash, snapshot)",
        )
        .bind(&ids)
        .bind(&revisions)
        .bind(&commits)
        .bind(&hashes)
        .bind(&snapshots)
        .execute(&mut *self.tx)
        .await?;

        let changes: Vec<String> = writes
            .iter()
            .map(|w| w.change.as_str().to_owned())
            .collect();
        let max: Option<i64> = sqlx::query(
            "WITH inserted AS (
                 INSERT INTO changes (item_id, scope, revision, change, commit_id)
                 SELECT u.id, u.scope, u.revision, u.change, u.commit_id
                 FROM UNNEST($1::text[], $2::text[], $3::int4[], $4::text[], $5::text[]) WITH ORDINALITY
                      AS u(id, scope, revision, change, commit_id, ord)
                 ORDER BY u.ord
                 RETURNING seq
             )
             SELECT max(seq) FROM inserted",
        )
        .bind(&ids)
        .bind(&scopes)
        .bind(&revisions)
        .bind(&changes)
        .bind(&commits)
        .fetch_one(&mut *self.tx)
        .await?
        .get(0);
        self.max_seq = self.max_seq.max(max);
        Ok(())
    }

    /// 记下索引到的提交，有变更时发通知，提交事务。
    pub async fn finish(mut self, commit: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO index_state (singleton, commit_id) VALUES (true, $1)
             ON CONFLICT (singleton) DO UPDATE SET commit_id = EXCLUDED.commit_id, indexed_at = now()",
        )
        .bind(commit)
        .execute(&mut *self.tx)
        .await?;
        if let Some(seq) = self.max_seq {
            let payload =
                serde_json::json!({ "schema": self.schema, "seq": seq, "commit": commit })
                    .to_string();
            sqlx::query("SELECT pg_notify($1, $2)")
                .bind(NOTIFY_CHANNEL)
                .bind(payload)
                .execute(&mut *self.tx)
                .await?;
        }
        self.tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_names() {
        assert!(valid_schema("tcms"));
        assert!(valid_schema("tcms_test_1"));
        assert!(!valid_schema("Tcms"));
        assert!(!valid_schema("1tcms"));
        assert!(!valid_schema("tcms; drop"));
        assert!(!valid_schema(""));
    }
}
