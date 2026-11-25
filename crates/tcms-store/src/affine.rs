//! 和 AFFiNE 同步的几张表（迁移 0007，`docs/affine-sync.md` 5.1）。只有同步程序用。

use serde::Serialize;
use sqlx::Row;

use crate::{Result, Store};

/// 一条和 AFFiNE 同步有关的审计（`affine.sync`、`affine.link`、`affine.resolve`、`affine.resume`）。时间是 UTC。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AffineAudit {
    pub at: String,
    pub actor: String,
    pub action: String,
    /// 文件路径（`affine.sync` 是工作区 id）。
    pub target: String,
    pub detail: serde_json::Value,
}

/// 接上的范围。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AffineScope {
    /// `org`、`team:x`……
    pub scope: String,
    pub workspace_id: String,
    pub folder_id: String,
    /// 整个范围都接上了（新文件、新文档也同步）。
    pub whole: bool,
}

/// 接上的一个文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffineDoc {
    pub path: String,
    pub scope: String,
    pub workspace_id: String,
    pub doc_id: String,
    /// 上次同步完整篇文档的 Yjs 状态。
    pub state: Vec<u8>,
    pub git_blob: Option<String>,
    pub synced_commit: Option<String>,
    pub paused: Option<String>,
    /// 上次同步的时间（RFC 3339）。存的时候不用填。
    pub synced_at: String,
}

/// 一条的对应关系和底稿。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffineBase {
    pub item_id: String,
    pub path: String,
    pub heading_id: String,
    pub title: String,
    pub body: String,
}

/// 等人处理的一条。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AffineConflict {
    /// 存的时候不用填。
    pub id: i64,
    pub path: String,
    pub item_id: Option<String>,
    pub heading_id: Option<String>,
    pub reason: String,
    pub message: Option<String>,
    pub git_title: Option<String>,
    pub git_body: Option<String>,
    pub affine_title: Option<String>,
    pub affine_body: Option<String>,
    pub base_title: Option<String>,
    pub base_body: Option<String>,
    /// 存的时候不用填。
    pub found_at: String,
}

/// 同步完一篇文档以后一起存的。
#[derive(Debug, Clone)]
pub struct AffineSave {
    pub doc: AffineDoc,
    /// 新的或改了的底稿。
    pub set: Vec<AffineBase>,
    /// 去掉这些条目的底稿（只去掉还在这个文件下的）。
    pub remove: Vec<String>,
    pub conflicts: Vec<AffineConflict>,
}

fn doc_of(r: &sqlx::postgres::PgRow) -> Result<AffineDoc> {
    Ok(AffineDoc {
        path: r.try_get("path")?,
        scope: r.try_get("scope")?,
        workspace_id: r.try_get("workspace_id")?,
        doc_id: r.try_get("doc_id")?,
        state: r.try_get("state")?,
        git_blob: r.try_get("git_blob")?,
        synced_commit: r.try_get("synced_commit")?,
        paused: r.try_get("paused")?,
        synced_at: r.try_get("synced_at")?,
    })
}

impl Store {
    /// 接上的范围，按范围排。
    pub async fn affine_scopes(&self) -> Result<Vec<AffineScope>> {
        let rows = sqlx::query(
            "SELECT scope, workspace_id, folder_id, whole FROM affine_scopes ORDER BY scope",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(AffineScope {
                    scope: r.try_get("scope")?,
                    workspace_id: r.try_get("workspace_id")?,
                    folder_id: r.try_get("folder_id")?,
                    whole: r.try_get("whole")?,
                })
            })
            .collect()
    }

    /// 记下（或改掉）一个范围对应的文件夹。
    pub async fn put_affine_scope(&self, s: &AffineScope) -> Result<()> {
        sqlx::query(
            "INSERT INTO affine_scopes (scope, workspace_id, folder_id, whole) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (scope) DO UPDATE SET workspace_id = EXCLUDED.workspace_id, \
                 folder_id = EXCLUDED.folder_id, whole = EXCLUDED.whole",
        )
        .bind(&s.scope)
        .bind(&s.workspace_id)
        .bind(&s.folder_id)
        .bind(s.whole)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 接上的文件，按路径排。
    pub async fn affine_docs(&self) -> Result<Vec<AffineDoc>> {
        let rows = sqlx::query(
            "SELECT path, scope, workspace_id, doc_id, state, git_blob, synced_commit, paused, \
                    to_char(synced_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS synced_at \
             FROM affine_docs ORDER BY path",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(doc_of).collect()
    }

    /// 所有条目的底稿，按文件、条目排。
    pub async fn affine_bases(&self) -> Result<Vec<AffineBase>> {
        let rows = sqlx::query(
            "SELECT item_id, path, heading_id, base_title, base_body FROM affine_items ORDER BY path, item_id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(AffineBase {
                    item_id: r.try_get("item_id")?,
                    path: r.try_get("path")?,
                    heading_id: r.try_get("heading_id")?,
                    title: r.try_get("base_title")?,
                    body: r.try_get("base_body")?,
                })
            })
            .collect()
    }

    /// 等人处理的，早的在前。
    pub async fn affine_conflicts(&self) -> Result<Vec<AffineConflict>> {
        let rows = sqlx::query(
            "SELECT id, path, item_id, heading_id, reason, message, git_title, git_body, affine_title, \
                    affine_body, base_title, base_body, \
                    to_char(found_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS found_at \
             FROM affine_conflicts ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(AffineConflict {
                    id: r.try_get("id")?,
                    path: r.try_get("path")?,
                    item_id: r.try_get("item_id")?,
                    heading_id: r.try_get("heading_id")?,
                    reason: r.try_get("reason")?,
                    message: r.try_get("message")?,
                    git_title: r.try_get("git_title")?,
                    git_body: r.try_get("git_body")?,
                    affine_title: r.try_get("affine_title")?,
                    affine_body: r.try_get("affine_body")?,
                    base_title: r.try_get("base_title")?,
                    base_body: r.try_get("base_body")?,
                    found_at: r.try_get("found_at")?,
                })
            })
            .collect()
    }

    /// 存一篇文档这次同步的结果：文档的状态、底稿、新记下的冲突，在一个事务里。
    pub async fn save_affine_doc(&self, save: &AffineSave) -> Result<()> {
        let d = &save.doc;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO affine_docs (path, scope, workspace_id, doc_id, state, git_blob, synced_commit, paused, synced_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now()) \
             ON CONFLICT (path) DO UPDATE SET scope = EXCLUDED.scope, workspace_id = EXCLUDED.workspace_id, \
                 doc_id = EXCLUDED.doc_id, state = EXCLUDED.state, git_blob = EXCLUDED.git_blob, \
                 synced_commit = EXCLUDED.synced_commit, paused = EXCLUDED.paused, synced_at = now()",
        )
        .bind(&d.path)
        .bind(&d.scope)
        .bind(&d.workspace_id)
        .bind(&d.doc_id)
        .bind(&d.state)
        .bind(&d.git_blob)
        .bind(&d.synced_commit)
        .bind(&d.paused)
        .execute(&mut *tx)
        .await?;
        if !save.remove.is_empty() {
            sqlx::query("DELETE FROM affine_items WHERE path = $1 AND item_id = ANY($2)")
                .bind(&d.path)
                .bind(&save.remove)
                .execute(&mut *tx)
                .await?;
        }
        for b in &save.set {
            // 同一个标题块原来对应别的条目（比如处理冲突时换了对应关系）：先去掉旧的。
            sqlx::query(
                "DELETE FROM affine_items WHERE path = $1 AND heading_id = $2 AND item_id <> $3",
            )
            .bind(&b.path)
            .bind(&b.heading_id)
            .bind(&b.item_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO affine_items (item_id, path, heading_id, base_title, base_body) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (item_id) DO UPDATE SET path = EXCLUDED.path, heading_id = EXCLUDED.heading_id, \
                     base_title = EXCLUDED.base_title, base_body = EXCLUDED.base_body",
            )
            .bind(&b.item_id)
            .bind(&b.path)
            .bind(&b.heading_id)
            .bind(&b.title)
            .bind(&b.body)
            .execute(&mut *tx)
            .await?;
        }
        for c in &save.conflicts {
            sqlx::query(
                "INSERT INTO affine_conflicts (path, item_id, heading_id, reason, message, git_title, git_body, \
                     affine_title, affine_body, base_title, base_body) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) ON CONFLICT DO NOTHING",
            )
            .bind(&c.path)
            .bind(&c.item_id)
            .bind(&c.heading_id)
            .bind(&c.reason)
            .bind(&c.message)
            .bind(&c.git_title)
            .bind(&c.git_body)
            .bind(&c.affine_title)
            .bind(&c.affine_body)
            .bind(&c.base_title)
            .bind(&c.base_body)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 停下（或接着同步）一篇文档。
    pub async fn set_affine_paused(&self, path: &str, reason: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE affine_docs SET paused = $2 WHERE path = $1")
            .bind(path)
            .bind(reason)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 不再同步这个文件（底稿跟着删掉，等人处理的也删掉）。
    pub async fn remove_affine_doc(&self, path: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM affine_conflicts WHERE path = $1")
            .bind(path)
            .execute(&mut *tx)
            .await?;
        let n = sqlx::query("DELETE FROM affine_docs WHERE path = $1")
            .bind(path)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(n > 0)
    }

    /// 处理完一条：删掉它，按处理的结果改底稿（`set` 换成新的，`remove` 去掉），记进审计
    /// （`affine.resolve`，对象是文件路径，`detail` 由调用的一方给）。
    pub async fn resolve_affine_conflict(
        &self,
        id: i64,
        set: Option<&AffineBase>,
        remove: Option<(&str, &str)>,
        actor: &str,
        detail: serde_json::Value,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let path: Option<String> =
            sqlx::query_scalar("DELETE FROM affine_conflicts WHERE id = $1 RETURNING path")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(path) = path else {
            return Ok(false);
        };
        // 下次同步时这篇文档不能因为“两边都没变”跳过。
        sqlx::query("UPDATE affine_docs SET git_blob = NULL WHERE path = $1")
            .bind(&path)
            .execute(&mut *tx)
            .await?;
        if let Some((path, item)) = remove {
            sqlx::query("DELETE FROM affine_items WHERE path = $1 AND item_id = $2")
                .bind(path)
                .bind(item)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(b) = set {
            sqlx::query(
                "DELETE FROM affine_items WHERE path = $1 AND heading_id = $2 AND item_id <> $3",
            )
            .bind(&b.path)
            .bind(&b.heading_id)
            .bind(&b.item_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO affine_items (item_id, path, heading_id, base_title, base_body) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (item_id) DO UPDATE SET path = EXCLUDED.path, heading_id = EXCLUDED.heading_id, \
                     base_title = EXCLUDED.base_title, base_body = EXCLUDED.base_body",
            )
            .bind(&b.item_id)
            .bind(&b.path)
            .bind(&b.heading_id)
            .bind(&b.title)
            .bind(&b.body)
            .execute(&mut *tx)
            .await?;
        }
        crate::tokens::audit(&mut tx, actor, "affine.resolve", &path, detail).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// 记一条和 AFFiNE 同步有关的审计（不在别的事务里的，比如审核页里让停着的文档接着做：`affine.resume`）。
    pub async fn record_affine_audit(
        &self,
        actor: &str,
        action: &str,
        target: &str,
        detail: serde_json::Value,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        crate::tokens::audit(&mut tx, actor, action, target, detail).await?;
        tx.commit().await?;
        Ok(())
    }

    /// 和 AFFiNE 同步有关的审计（`affine.` 开头的），新的在前，最多 `limit` 条。
    pub async fn affine_audit(&self, limit: i64) -> Result<Vec<AffineAudit>> {
        let rows = sqlx::query(
            "SELECT to_char(at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS at, actor, action, \
                 coalesce(target, '') AS target, detail \
             FROM audit WHERE action LIKE 'affine.%' ORDER BY seq DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(AffineAudit {
                    at: r.try_get("at")?,
                    actor: r.try_get("actor")?,
                    action: r.try_get("action")?,
                    target: r.try_get("target")?,
                    detail: r.try_get("detail")?,
                })
            })
            .collect()
    }
}
