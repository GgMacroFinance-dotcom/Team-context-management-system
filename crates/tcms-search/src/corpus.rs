//! 内存里的生效条目，和“谁能看到哪些范围”。

use std::cmp::Reverse;

use serde::Serialize;
use tcms_core::{Kind, Scope, valid_name};
use tcms_store::{Snapshot, Store};

use crate::Error;

/// 一条生效的条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CorpusItem {
    pub id: String,
    pub scope: Scope,
    pub kind: Kind,
    pub authority: i16,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub owner: Option<String>,
    pub review_by: Option<String>,
    pub supersedes: Option<String>,
    pub revision: i32,
    pub path: String,
    pub updated_ms: i64,
}

impl CorpusItem {
    /// 同分时的顺序：地位高的（数字小）在前，更新近的在前，再按 id。
    pub fn tie_key(&self) -> (i16, Reverse<i64>, &str) {
        (self.authority, Reverse(self.updated_ms), &self.id)
    }

    pub fn pinned(&self) -> bool {
        self.tags.iter().any(|t| t == "pinned")
    }
}

/// 某一时刻的全部生效条目，按 id 排。
#[derive(Debug, Clone, Default)]
pub struct Corpus {
    items: Vec<CorpusItem>,
    /// 取条目时的变更序号（`context_changes` 从这里接着问）。
    pub cursor: i64,
    /// 索引到的提交。
    pub commit: Option<String>,
}

impl Corpus {
    pub fn new(mut items: Vec<CorpusItem>, cursor: i64, commit: Option<String>) -> Self {
        items.sort_by(|a, b| a.id.cmp(&b.id));
        Self {
            items,
            cursor,
            commit,
        }
    }

    /// 从 PostgreSQL 取一份。
    pub async fn load(store: &Store) -> Result<Self, Error> {
        Self::from_snapshot(store.snapshot().await?)
    }

    pub fn from_snapshot(s: Snapshot) -> Result<Self, Error> {
        let items = s
            .items
            .into_iter()
            .map(|a| {
                let r = a.row;
                let bad = |what: &str| Error::BadRow(format!("{} 的{what}", r.id));
                Ok(CorpusItem {
                    scope: r.scope.parse().map_err(|_| bad("范围"))?,
                    kind: r.kind.parse().map_err(|_| bad("类型"))?,
                    id: r.id,
                    authority: r.authority,
                    title: r.title,
                    body: r.body,
                    tags: r.tags,
                    owner: r.owner,
                    review_by: r.review_by,
                    supersedes: r.supersedes,
                    revision: r.revision,
                    path: r.path,
                    updated_ms: a.updated_ms,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(Self::new(items, s.cursor, s.commit))
    }

    pub fn items(&self) -> &[CorpusItem] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<&CorpusItem> {
        self.items
            .binary_search_by(|it| it.id.as_str().cmp(id))
            .ok()
            .map(|i| &self.items[i])
    }
}

/// 调用者能看到的范围（`docs/spec.md` 3.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visibility {
    /// 人（审核的人、命令行）看得到全部。
    All,
    /// agent：`org`、所在团队、自己的岗位、这次任务声明的项目。
    Scopes(Vec<Scope>),
}

impl Visibility {
    pub fn agent(role: &str, team: Option<&str>, project: Option<&str>) -> Result<Self, Error> {
        let check = |what: &str, name: &str| {
            if valid_name(name) {
                Ok(())
            } else {
                Err(Error::BadName(format!("{what} {name}")))
            }
        };
        check("岗位", role)?;
        let mut scopes = vec![Scope::Org, Scope::Role(role.to_owned())];
        if let Some(t) = team {
            check("团队", t)?;
            scopes.push(Scope::Team(t.to_owned()));
        }
        if let Some(p) = project {
            check("项目", p)?;
            scopes.push(Scope::Project(p.to_owned()));
        }
        scopes.sort();
        Ok(Self::Scopes(scopes))
    }

    pub fn allows(&self, scope: &Scope) -> bool {
        match self {
            Visibility::All => true,
            Visibility::Scopes(s) => s.contains(scope),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_visibility() {
        let v = Visibility::agent("macro", Some("research"), Some("q4")).unwrap();
        assert!(v.allows(&Scope::Org));
        assert!(v.allows(&Scope::Team("research".into())));
        assert!(v.allows(&Scope::Role("macro".into())));
        assert!(v.allows(&Scope::Project("q4".into())));
        assert!(!v.allows(&Scope::Role("data".into())));
        assert!(!v.allows(&Scope::Team("ops".into())));
        assert!(!v.allows(&Scope::Project("q1".into())));
        let v = Visibility::agent("macro", None, None).unwrap();
        assert!(!v.allows(&Scope::Team("research".into())));
        assert!(Visibility::agent("a b", None, None).is_err());
        assert!(Visibility::All.allows(&Scope::Team("x".into())));
    }
}
