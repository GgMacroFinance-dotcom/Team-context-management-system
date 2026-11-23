//! 提议（`docs/spec.md` 第 5 节）：agent 提、人批准或拒绝。批准时写 git 的部分在 `tcms-write`，
//! 这里只管表：建（挡重复、挡太多）、列、读，和批准、拒绝时锁住提交队列。

use std::fmt;
use std::str::FromStr;

use serde::Serialize;
use sqlx::postgres::PgRow;
use sqlx::{Postgres, Row, Transaction};

use crate::tokens::audit;
use crate::{Identity, Result, Store};

/// 提议做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// 新建一条。
    Create,
    /// 修改一条（id 不变，版本号加一）。
    Update,
    /// 用新的一条取代旧的（新的写 `supersedes=旧的`，旧的变成 superseded）。
    Supersede,
    /// 归档（从文件里删掉那一条）。
    Archive,
}

impl Action {
    pub const ALL: [Action; 4] = [
        Action::Create,
        Action::Update,
        Action::Supersede,
        Action::Archive,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Action::Create => "create",
            Action::Update => "update",
            Action::Supersede => "supersede",
            Action::Archive => "archive",
        }
    }

    /// 要不要指定一条已有的。
    pub fn needs_item(self) -> bool {
        self != Action::Create
    }
}

impl FromStr for Action {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Action::ALL
            .into_iter()
            .find(|a| a.as_str() == s)
            .ok_or_else(|| format!("不认识的 action {s}，可选：create、update、supersede、archive"))
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 要存的一条提议（已经检查、脱敏过）。
#[derive(Debug, Clone)]
pub struct NewProposal {
    pub action: Action,
    pub item_id: Option<String>,
    pub base_revision: Option<i32>,
    pub scope: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub review_by: Option<String>,
    pub reason: String,
    pub evidence: Vec<String>,
    pub conflicts: serde_json::Value,
    pub content_hash: String,
    pub redacted: i32,
}

/// 表里的一条提议。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProposalRow {
    pub id: String,
    pub action: String,
    pub item_id: Option<String>,
    pub base_revision: Option<i32>,
    pub scope: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub review_by: Option<String>,
    pub reason: String,
    pub evidence: Vec<String>,
    pub conflicts: serde_json::Value,
    pub content_hash: String,
    pub redacted: i32,
    pub proposer_token: String,
    pub proposer_name: String,
    pub proposer_role: String,
    pub status: String,
    pub created_at: String,
    pub decided_at: Option<String>,
    pub decided_by: Option<String>,
    pub decision_note: Option<String>,
    pub result_item: Option<String>,
    pub result_commit: Option<String>,
}

/// 取整条提议的 SELECT，后面接条件。写成宏是为了拼出来的仍是字面量。
macro_rules! select_proposal {
    ($tail:literal) => {
        concat!(
            "SELECT id, action, item_id, base_revision, scope, kind, title, body, tags, review_by, ",
            "reason, evidence, conflicts, content_hash, redacted, proposer_token, proposer_name, proposer_role, ",
            "status, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS'), ",
            "to_char(decided_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS'), ",
            "decided_by, decision_note, result_item, result_commit FROM proposals ",
            $tail
        )
    };
}

fn row(r: &PgRow) -> ProposalRow {
    ProposalRow {
        id: r.get(0),
        action: r.get(1),
        item_id: r.get(2),
        base_revision: r.get(3),
        scope: r.get(4),
        kind: r.get(5),
        title: r.get(6),
        body: r.get(7),
        tags: r.get(8),
        review_by: r.get(9),
        reason: r.get(10),
        evidence: r.get(11),
        conflicts: r.get(12),
        content_hash: r.get(13),
        redacted: r.get(14),
        proposer_token: r.get(15),
        proposer_name: r.get(16),
        proposer_role: r.get(17),
        status: r.get(18),
        created_at: r.get(19),
        decided_at: r.get(20),
        decided_by: r.get(21),
        decision_note: r.get(22),
        result_item: r.get(23),
        result_commit: r.get(24),
    }
}

/// 建提议的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Proposed {
    New(ProposalRow),
    /// 一样的内容已经有一条没处理的（可能是别的 agent 提的），不再建。
    Duplicate(ProposalRow),
    /// 这个令牌没处理的提议已经到上限。
    TooMany {
        open: i64,
    },
}

impl Store {
    /// 建一条提议。同一个令牌的提议排队建（锁令牌那一行），重复的、超过 `max_open` 条的不建。
    pub async fn create_proposal(
        &self,
        who: &Identity,
        p: &NewProposal,
        max_open: i64,
    ) -> Result<Proposed> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT 1 FROM tokens WHERE id = $1 FOR UPDATE")
            .bind(&who.token_id)
            .execute(&mut *tx)
            .await?;
        if let Some(dup) = open_with_hash(&mut tx, &p.content_hash).await? {
            return Ok(Proposed::Duplicate(dup));
        }
        let open: i64 = sqlx::query(
            "SELECT count(*) FROM proposals WHERE proposer_token = $1 AND status = 'open'",
        )
        .bind(&who.token_id)
        .fetch_one(&mut *tx)
        .await?
        .get(0);
        if open >= max_open {
            return Ok(Proposed::TooMany { open });
        }
        let id = format!("prop_{}", ulid::Ulid::generate());
        let inserted = sqlx::query(
            "INSERT INTO proposals (id, action, item_id, base_revision, scope, kind, title, body, tags,
                review_by, reason, evidence, conflicts, content_hash, redacted,
                proposer_token, proposer_name, proposer_role)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
             ON CONFLICT (content_hash) WHERE status = 'open' DO NOTHING",
        )
        .bind(&id)
        .bind(p.action.as_str())
        .bind(&p.item_id)
        .bind(p.base_revision)
        .bind(&p.scope)
        .bind(&p.kind)
        .bind(&p.title)
        .bind(&p.body)
        .bind(&p.tags)
        .bind(&p.review_by)
        .bind(&p.reason)
        .bind(&p.evidence)
        .bind(&p.conflicts)
        .bind(&p.content_hash)
        .bind(p.redacted)
        .bind(&who.token_id)
        .bind(&who.name)
        .bind(&who.role)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if !inserted {
            // 别的令牌同时提了一样的。
            let dup = open_with_hash(&mut tx, &p.content_hash).await?;
            tx.rollback().await?;
            return Ok(match dup {
                Some(d) => Proposed::Duplicate(d),
                None => Proposed::TooMany { open },
            });
        }
        let detail = serde_json::json!({
            "action": p.action.as_str(), "item_id": p.item_id, "scope": p.scope,
            "title": p.title, "token": who.token_id, "redacted": p.redacted,
        });
        audit(&mut tx, &agent_actor(who), "proposal.create", &id, detail).await?;
        let created = sqlx::query(select_proposal!("WHERE id = $1"))
            .bind(&id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Proposed::New(row(&created)))
    }

    pub async fn get_proposal(&self, id: &str) -> Result<Option<ProposalRow>> {
        Ok(sqlx::query(select_proposal!("WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(row))
    }

    /// 按状态列（`None` 是全部），先提的在前。
    pub async fn list_proposals(&self, status: Option<&str>) -> Result<Vec<ProposalRow>> {
        let rows = sqlx::query(select_proposal!(
            "WHERE $1::text IS NULL OR status = $1 ORDER BY created_at, id"
        ))
        .bind(status)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row).collect())
    }

    /// 开始处理一条提议：拿到提交队列的锁（一个 schema 同一时间只处理一条），锁住这一行。
    /// 没有这条提议时返回 `None`。拿到的 [`Decision`] 放掉（没有 accept/reject）就是什么都没做。
    pub async fn begin_decision(&self, id: &str) -> Result<Option<Decision>> {
        let queue = self.commit_queue.clone().lock_owned().await;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended('tcms_commit:' || current_schema(), 0))",
        )
        .execute(&mut *tx)
        .await?;
        let found = sqlx::query(select_proposal!("WHERE id = $1 FOR UPDATE"))
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        Ok(found.map(|r| Decision {
            tx,
            _queue: queue,
            proposal: row(&r),
        }))
    }
}

/// 审计里 agent 的写法。
fn agent_actor(who: &Identity) -> String {
    format!("{}（{}）", who.name, who.token_id)
}

async fn open_with_hash(
    tx: &mut Transaction<'_, Postgres>,
    hash: &str,
) -> Result<Option<ProposalRow>> {
    Ok(sqlx::query(select_proposal!(
        "WHERE content_hash = $1 AND status = 'open'"
    ))
    .bind(hash)
    .fetch_optional(&mut **tx)
    .await?
    .as_ref()
    .map(row))
}

/// 正在处理的一条提议，拿着提交队列的锁。
pub struct Decision {
    tx: Transaction<'static, Postgres>,
    _queue: tokio::sync::OwnedMutexGuard<()>,
    pub proposal: ProposalRow,
}

impl Decision {
    /// 记下接受了：写成了 `item`，提交 `commit`。
    pub async fn accept(
        mut self,
        actor: &str,
        note: Option<&str>,
        item: &str,
        commit: &str,
    ) -> Result<ProposalRow> {
        sqlx::query(
            "UPDATE proposals SET status = 'accepted', decided_at = now(), decided_by = $2,
                decision_note = $3, result_item = $4, result_commit = $5 WHERE id = $1",
        )
        .bind(&self.proposal.id)
        .bind(actor)
        .bind(note)
        .bind(item)
        .bind(commit)
        .execute(&mut *self.tx)
        .await?;
        let detail = serde_json::json!({"item": item, "commit": commit, "note": note});
        audit(
            &mut self.tx,
            actor,
            "proposal.accept",
            &self.proposal.id,
            detail,
        )
        .await?;
        let r = sqlx::query(select_proposal!("WHERE id = $1"))
            .bind(&self.proposal.id)
            .fetch_one(&mut *self.tx)
            .await?;
        self.tx.commit().await?;
        Ok(row(&r))
    }

    /// 记下拒绝了。
    pub async fn reject(mut self, actor: &str, note: Option<&str>) -> Result<()> {
        sqlx::query(
            "UPDATE proposals SET status = 'rejected', decided_at = now(), decided_by = $2,
                decision_note = $3 WHERE id = $1",
        )
        .bind(&self.proposal.id)
        .bind(actor)
        .bind(note)
        .execute(&mut *self.tx)
        .await?;
        let detail = serde_json::json!({"note": note});
        audit(
            &mut self.tx,
            actor,
            "proposal.reject",
            &self.proposal.id,
            detail,
        )
        .await?;
        self.tx.commit().await?;
        Ok(())
    }
}
