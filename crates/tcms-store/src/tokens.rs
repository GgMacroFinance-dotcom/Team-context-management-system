//! 令牌、审计，和按可见范围查变更（MCP 服务端用）。

use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::Row;
use sqlx::postgres::PgListener;

use crate::{Error, NOTIFY_CHANNEL, Result, Store};

/// 令牌明文的前缀，方便认出来（比如在日志、密钥扫描里）。
pub const TOKEN_PREFIX: &str = "tcms_";
/// 能声明任何项目。
pub const ALL_PROJECTS: &str = "*";

/// 令牌明文的 SHA-256（十六进制）。令牌本身是 32 字节随机数，不需要加盐。
pub fn hash_token(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn new_secret() -> Result<String> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| Error::Random(e.to_string()))?;
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{TOKEN_PREFIX}{hex}"))
}

/// 新建一个令牌要填的。
#[derive(Debug, Clone)]
pub struct NewToken {
    /// 谁在用（agent 的名字），只给人看。
    pub name: String,
    pub role: String,
    pub team: Option<String>,
    /// 能声明的项目；含 `*` 表示全部。
    pub projects: Vec<String>,
    /// 多少天后过期；`None` 表示不过期。
    pub expires_in_days: Option<i32>,
}

/// 令牌背后的身份。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Identity {
    pub token_id: String,
    pub name: String,
    pub role: String,
    pub team: Option<String>,
    pub projects: Vec<String>,
}

impl Identity {
    pub fn all_projects(&self) -> bool {
        self.projects.iter().any(|p| p == ALL_PROJECTS)
    }

    /// 能不能声明这个项目。
    pub fn may_use_project(&self, project: &str) -> bool {
        self.all_projects() || self.projects.iter().any(|p| p == project)
    }

    /// 能看的范围（`org`、团队、岗位、允许的项目），存库时的写法。`*` 不在里面，看 [`Identity::all_projects`]。
    pub fn scope_strings(&self) -> Vec<String> {
        let mut s = vec!["org".to_owned(), format!("role:{}", self.role)];
        if let Some(t) = &self.team {
            s.push(format!("team:{t}"));
        }
        s.extend(
            self.projects
                .iter()
                .filter(|p| p.as_str() != ALL_PROJECTS)
                .map(|p| format!("project:{p}")),
        );
        s
    }
}

/// `tcms token list` 的一行。
#[derive(Debug, Clone, Serialize)]
pub struct TokenRow {
    pub id: String,
    pub name: String,
    pub role: String,
    pub team: Option<String>,
    pub projects: Vec<String>,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    /// 过了过期时间。
    pub expired: bool,
}

/// 调用者看得到的一条变更。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VisibleChange {
    pub seq: i64,
    pub id: String,
    pub scope: String,
    pub revision: i32,
    pub change: String,
    pub commit: String,
    /// 这一条现在的标题（条目还在索引里时）。
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VisibleChanges {
    pub changes: Vec<VisibleChange>,
    /// 下次从这里接着问。没有更多时是当时最大的变更序号（看不到的变更也跳过去）。
    pub cursor: i64,
    /// 还有没返回的变更。
    pub more: bool,
}

impl Store {
    /// 建一个令牌，返回 `(id, 明文)`。明文只有这一次能看到。
    pub async fn create_token(&self, t: &NewToken, actor: &str) -> Result<(String, String)> {
        let id = format!("tok_{}", ulid::Ulid::generate());
        let secret = new_secret()?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO tokens (id, name, token_hash, role, team, projects, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, CASE WHEN $7::int4 IS NULL THEN NULL ELSE now() + make_interval(days => $7) END)",
        )
        .bind(&id)
        .bind(&t.name)
        .bind(hash_token(&secret))
        .bind(&t.role)
        .bind(&t.team)
        .bind(&t.projects)
        .bind(t.expires_in_days)
        .execute(&mut *tx)
        .await?;
        let detail = serde_json::json!({
            "name": t.name, "role": t.role, "team": t.team,
            "projects": t.projects, "expires_in_days": t.expires_in_days,
        });
        audit(&mut tx, actor, "token.create", &id, detail).await?;
        tx.commit().await?;
        Ok((id, secret))
    }

    /// 明文令牌 → 身份。不存在、已吊销、已过期都返回 `None`。
    pub async fn identify(&self, secret: &str) -> Result<Option<Identity>> {
        let row = sqlx::query(
            "SELECT id, name, role, team, projects FROM tokens
             WHERE token_hash = $1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(hash_token(secret))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| Identity {
            token_id: r.get(0),
            name: r.get(1),
            role: r.get(2),
            team: r.get(3),
            projects: r.get(4),
        }))
    }

    pub async fn list_tokens(&self) -> Result<Vec<TokenRow>> {
        let rows = sqlx::query(
            "SELECT id, name, role, team, projects,
                    to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI'),
                    to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI'),
                    to_char(revoked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI'),
                    coalesce(expires_at <= now(), false)
             FROM tokens ORDER BY created_at, id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| TokenRow {
                id: r.get(0),
                name: r.get(1),
                role: r.get(2),
                team: r.get(3),
                projects: r.get(4),
                created_at: r.get(5),
                expires_at: r.get(6),
                revoked_at: r.get(7),
                expired: r.get(8),
            })
            .collect())
    }

    /// 吊销一个令牌。返回有没有吊销（不存在或早已吊销时是 `false`）。
    pub async fn revoke_token(&self, id: &str, actor: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let done = sqlx::query(
            "UPDATE tokens SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if done {
            audit(&mut tx, actor, "token.revoke", id, serde_json::json!({})).await?;
        }
        tx.commit().await?;
        Ok(done)
    }

    /// 当前最大的变更序号，没有变更时是 0。
    pub async fn latest_cursor(&self) -> Result<i64> {
        Ok(
            sqlx::query("SELECT coalesce(max(seq), 0)::int8 FROM changes")
                .fetch_one(&self.pool)
                .await?
                .get(0),
        )
    }

    /// `context_changes`：序号大于 `after`、范围在 `scopes` 里（`all_projects` 时所有项目也算）的变更，最多 `limit` 条。
    pub async fn visible_changes(
        &self,
        after: i64,
        scopes: &[String],
        all_projects: bool,
        limit: usize,
    ) -> Result<VisibleChanges> {
        let limit = limit.max(1);
        let mut tx = self
            .pool
            .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .await?;
        let rows = sqlx::query(
            "SELECT c.seq, c.item_id, c.scope, c.revision, c.change, c.commit_id, i.title
             FROM changes c LEFT JOIN items i ON i.id = c.item_id
             WHERE c.seq > $1 AND (c.scope = ANY($2) OR ($3 AND c.scope LIKE 'project:%'))
             ORDER BY c.seq LIMIT $4",
        )
        .bind(after)
        .bind(scopes)
        .bind(all_projects)
        .bind(limit as i64 + 1)
        .fetch_all(&mut *tx)
        .await?;
        let latest: i64 = sqlx::query("SELECT coalesce(max(seq), 0)::int8 FROM changes")
            .fetch_one(&mut *tx)
            .await?
            .get(0);
        tx.commit().await?;
        let more = rows.len() > limit;
        let changes: Vec<VisibleChange> = rows
            .iter()
            .take(limit)
            .map(|r| VisibleChange {
                seq: r.get(0),
                id: r.get(1),
                scope: r.get(2),
                revision: r.get(3),
                change: r.get(4),
                commit: r.get(5),
                title: r.get(6),
            })
            .collect();
        let cursor = if more {
            changes.last().map_or(after, |c| c.seq)
        } else {
            latest.max(after)
        };
        Ok(VisibleChanges {
            changes,
            cursor,
            more,
        })
    }

    /// 订阅变更通知（`NOTIFY tcms_changes`）。
    pub async fn listen(&self) -> Result<PgListener> {
        let mut l = PgListener::connect_with(&self.pool).await?;
        l.listen(NOTIFY_CHANNEL).await?;
        Ok(l)
    }
}

pub(crate) async fn audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: &str,
    action: &str,
    target: &str,
    detail: serde_json::Value,
) -> Result<()> {
    sqlx::query("INSERT INTO audit (actor, action, target, detail) VALUES ($1, $2, $3, $4)")
        .bind(actor)
        .bind(action)
        .bind(target)
        .bind(detail)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_and_hashes() {
        let a = new_secret().unwrap();
        let b = new_secret().unwrap();
        assert!(a.starts_with(TOKEN_PREFIX) && a.len() == TOKEN_PREFIX.len() + 64);
        assert_ne!(a, b);
        assert_eq!(hash_token(&a), hash_token(&a));
        assert_ne!(hash_token(&a), hash_token(&b));
        assert_eq!(
            hash_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn identity_scopes() {
        let mut id = Identity {
            token_id: "tok_x".into(),
            name: "宏观研究员".into(),
            role: "macro".into(),
            team: Some("research".into()),
            projects: vec!["q4".into()],
        };
        assert_eq!(
            id.scope_strings(),
            ["org", "role:macro", "team:research", "project:q4"]
        );
        assert!(id.may_use_project("q4") && !id.may_use_project("q1"));
        id.projects = vec![ALL_PROJECTS.into()];
        assert!(id.all_projects() && id.may_use_project("q1"));
        assert_eq!(id.scope_strings(), ["org", "role:macro", "team:research"]);
    }
}
