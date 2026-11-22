//! 索引器：把上下文仓库（git）里的条目同步到 PostgreSQL（`docs/architecture.md` 第 4 节）。
//!
//! 分三段：
//! 1. [`collect`]（同步，只读 git）：算出从上次索引的提交到 HEAD 哪些文件变了，只解析这些文件。
//! 2. [`plan`]（纯函数）：和库里现有的状态比，得出每条要写的新版本。
//! 3. [`reindex`]：在一个事务里拿锁、做计划、写入、记下索引到的提交。
//!
//! 同样的仓库和库，跑两遍结果一样：第二遍没有任何变更。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::Serialize;
use tcms_core::{ParsedItem, Status, TEAMS_FILE, Teams, Warning, parse_file, scope_of_path};
use tcms_git::{Change, ContextRepo, ObjectId};
use tcms_store::{ChangeKind, ItemRow, ItemState, ItemWrite, Store};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Git(#[from] tcms_git::Error),
    #[error(transparent)]
    Store(#[from] tcms_store::Error),
    #[error("库里记的索引提交 {0} 不是有效的提交 id")]
    BadIndexedCommit(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// 从 git 里读出来、要交给 [`plan`] 的东西。
#[derive(Debug, Default)]
pub struct Collected {
    pub commit: Option<ObjectId>,
    /// 这次是不是全量（解析了所有文件）。
    pub full: bool,
    /// 解析过或删掉了的条目文件。全量时不用，库里所有条目都算受影响。
    pub affected_paths: BTreeSet<String>,
    /// 解析出的、有 id 的条目，按路径和行号排好。
    pub items: Vec<ParsedItem>,
    pub warnings: Vec<Warning>,
    pub files_parsed: usize,
}

/// 读 git：`base` 是上次索引到的提交，为空或者在仓库里找不到时做全量。
pub fn collect(repo: &ContextRepo, base: Option<ObjectId>) -> Result<Collected> {
    let Some(head) = repo.head()? else {
        return Ok(Collected::default());
    };
    let base = base.filter(|b| repo.has_commit(*b));
    let files = repo.files(head)?;
    let mut out = Collected {
        commit: Some(head),
        full: base.is_none(),
        ..Default::default()
    };

    let to_parse: Vec<String> = match base {
        None => files
            .keys()
            .filter(|p| scope_of_path(p).is_some())
            .cloned()
            .collect(),
        Some(base) => {
            let mut parse = Vec::new();
            for change in repo.changes(Some(base), head)? {
                match change {
                    Change::Upserted(p) if scope_of_path(&p).is_some() => parse.push(p),
                    Change::Deleted(p) if scope_of_path(&p).is_some() => {
                        out.affected_paths.insert(p);
                    }
                    _ => {}
                }
            }
            parse
        }
    };

    let teams = match files.get(TEAMS_FILE) {
        Some(id) => {
            let text = String::from_utf8_lossy(&repo.read_blob(*id)?).into_owned();
            match Teams::parse(&text) {
                Ok(t) => Some(t),
                Err(e) => {
                    out.warnings.push(Warning {
                        path: TEAMS_FILE.to_owned(),
                        line: None,
                        message: format!("{e}；这次不检查范围有没有定义"),
                    });
                    None
                }
            }
        }
        None => None,
    };

    let mut undefined_dirs = BTreeSet::new();
    let mut seen: HashMap<String, (String, usize)> = HashMap::new();
    for path in to_parse {
        let bytes = repo.read_blob(files[&path])?;
        let Ok(text) = String::from_utf8(bytes) else {
            out.warnings.push(Warning {
                path: path.clone(),
                line: None,
                message: "不是 UTF-8 文本，跳过，库里原来的内容不动".to_owned(),
            });
            continue;
        };
        out.affected_paths.insert(path.clone());
        out.files_parsed += 1;
        let parsed = parse_file(&path, &text);
        out.warnings.extend(parsed.warnings);
        for item in parsed.items {
            if let Some(teams) = &teams
                && !teams.defines(&item.scope)
                && undefined_dirs.insert(item.scope.dir())
            {
                out.warnings.push(Warning {
                    path: item.scope.dir(),
                    line: None,
                    message: format!("范围 {} 没有在 {TEAMS_FILE} 里定义", item.scope),
                });
            }
            let Some(id) = &item.id else {
                out.warnings.push(Warning {
                    path: item.path.clone(),
                    line: Some(item.line),
                    message: format!(
                        "「{}」还没有 id，没有进索引；先运行 tcms assign-ids",
                        item.title
                    ),
                });
                continue;
            };
            if let Some((p, l)) = seen.get(id.as_str()) {
                out.warnings.push(Warning {
                    path: item.path.clone(),
                    line: Some(item.line),
                    message: format!("id {id} 和 {p} 第 {l} 行重复，这一条没有进索引"),
                });
                continue;
            }
            seen.insert(id.to_string(), (item.path.clone(), item.line));
            out.items.push(item);
        }
    }
    Ok(out)
}

/// 计划里的一个新版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Planned {
    /// 用解析出的内容写整条。
    Content {
        index: usize,
        revision: i32,
        status: Status,
        change: ChangeKind,
    },
    /// 内容没变，只改状态（被取代、恢复、归档）。要先从库里取出整条。
    StatusOnly {
        id: String,
        revision: i32,
        status: Status,
        change: ChangeKind,
    },
}

/// 和库里的状态比，算出要写的新版本。按 id 排序，结果是确定的。
/// 返回计划和计划中发现的问题（比如两个文件用了同一个 id）。
pub fn plan(states: &[ItemState], c: &Collected) -> (Vec<Planned>, Vec<Warning>) {
    let mut warnings = Vec::new();
    let db: HashMap<&str, &ItemState> = states.iter().map(|s| (s.id.as_str(), s)).collect();
    let affected = |path: &str| c.full || c.affected_paths.contains(path);

    // 这次之后还在仓库里的条目：没受影响的文件里原有的，加上这次解析出的。
    // 值是 (解析出的条目下标, 它取代了谁)。
    let mut present: BTreeMap<&str, (Option<usize>, Option<&str>)> = BTreeMap::new();
    for s in states {
        if s.status != Status::Archived.as_str() && !affected(&s.path) {
            present.insert(&s.id, (None, s.supersedes.as_deref()));
        }
    }
    for (index, item) in c.items.iter().enumerate() {
        let id = item
            .id
            .as_ref()
            .expect("collect 只留下有 id 的条目")
            .as_str();
        if let Some((None, _)) = present.get(id) {
            let other = db.get(id).map_or("", |s| s.path.as_str());
            warnings.push(Warning {
                path: item.path.clone(),
                line: Some(item.line),
                message: format!("id {id} 已经在 {other} 里用了，这一条没有进索引"),
            });
            continue;
        }
        present.insert(
            id,
            (Some(index), item.supersedes.as_ref().map(|s| s.as_str())),
        );
    }
    let superseded: HashSet<&str> = present.values().filter_map(|(_, s)| *s).collect();

    let ids: BTreeSet<&str> = present.keys().copied().chain(db.keys().copied()).collect();
    let mut out = Vec::new();
    for id in ids {
        let old = db.get(id);
        let new = present.get(id);
        let status = match new {
            None => Status::Archived,
            Some(_) if superseded.contains(id) => Status::Superseded,
            Some(_) => Status::Active,
        };
        match (old, new) {
            (None, Some((Some(index), _))) => out.push(Planned::Content {
                index: *index,
                revision: 1,
                status,
                change: ChangeKind::Created,
            }),
            (None, _) => {}
            (Some(o), Some((index, _))) => {
                let old_status = o.status.as_str();
                let status_changed = old_status != status.as_str();
                let content_changed = index.is_some_and(|i| {
                    let item = &c.items[i];
                    item.content_hash() != o.content_hash || item.path != o.path
                });
                let change = if old_status == Status::Archived.as_str() {
                    ChangeKind::Restored
                } else if content_changed {
                    ChangeKind::Updated
                } else if status == Status::Superseded {
                    ChangeKind::Superseded
                } else {
                    ChangeKind::Restored
                };
                if !(content_changed || status_changed) {
                    continue;
                }
                out.push(match index {
                    Some(i) => Planned::Content {
                        index: *i,
                        revision: o.revision + 1,
                        status,
                        change,
                    },
                    None => Planned::StatusOnly {
                        id: id.to_owned(),
                        revision: o.revision + 1,
                        status,
                        change,
                    },
                });
            }
            (Some(o), None) => {
                if o.status != Status::Archived.as_str() {
                    out.push(Planned::StatusOnly {
                        id: id.to_owned(),
                        revision: o.revision + 1,
                        status: Status::Archived,
                        change: ChangeKind::Archived,
                    });
                }
            }
        }
    }
    (out, warnings)
}

fn row_of(item: &ParsedItem, revision: i32, status: Status, commit: &str) -> ItemRow {
    ItemRow {
        id: item.id.as_ref().expect("有 id").to_string(),
        scope: item.scope.to_string(),
        kind: item.kind.as_str().to_owned(),
        authority: item.kind.authority(),
        title: item.title.clone(),
        body: item.body.clone(),
        tags: item.tags.clone(),
        owner: item.owner.clone(),
        review_by: item.review_by.clone(),
        supersedes: item.supersedes.as_ref().map(ToString::to_string),
        status: status.as_str().to_owned(),
        revision,
        content_hash: item.content_hash(),
        path: item.path.clone(),
        commit_id: commit.to_owned(),
    }
}

/// 一次索引的结果。
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub commit: Option<String>,
    pub full: bool,
    /// 库里已经是这个提交，什么都没做。
    pub up_to_date: bool,
    pub files_parsed: usize,
    pub items_parsed: usize,
    pub counts: BTreeMap<&'static str, usize>,
    pub warnings: Vec<Warning>,
    #[serde(skip)]
    pub took: Duration,
}

impl Report {
    pub fn changed(&self) -> usize {
        self.counts.values().sum()
    }
}

/// 把仓库 HEAD 同步进库。`full` 为真时解析所有文件（库里的版本号保留）。
/// 和别的索引进程撞上时（库里的提交在中途变了）会重新来一次。
pub async fn reindex(repo: &ContextRepo, store: &Store, full: bool) -> Result<Report> {
    match reindex_once(repo, store, full).await {
        Err(Error::Store(tcms_store::Error::Conflict { .. })) => {
            reindex_once(repo, store, full).await
        }
        other => other,
    }
}

async fn reindex_once(repo: &ContextRepo, store: &Store, full: bool) -> Result<Report> {
    let started = Instant::now();
    let indexed = store.indexed_commit().await?;
    let base = match (&indexed, full) {
        (Some(s), false) => {
            Some(ObjectId::from_hex(s.as_bytes()).map_err(|_| Error::BadIndexedCommit(s.clone()))?)
        }
        _ => None,
    };
    let head = repo.head()?;
    let mut report = Report {
        commit: head.map(|h| h.to_string()),
        ..Default::default()
    };
    if head.is_none() || (base.is_some() && base == head) {
        report.up_to_date = true;
        report.took = started.elapsed();
        return Ok(report);
    }

    let collected = collect(repo, base)?;
    let commit = collected.commit.expect("上面确认过有 HEAD").to_string();
    report.full = collected.full;
    report.files_parsed = collected.files_parsed;
    report.items_parsed = collected.items.len();

    let mut tx = store.begin_index(indexed.as_deref()).await?;
    let states = tx.states().await?;
    let (planned, plan_warnings) = plan(&states, &collected);
    let need: Vec<String> = planned
        .iter()
        .filter_map(|p| match p {
            Planned::StatusOnly { id, .. } => Some(id.clone()),
            Planned::Content { .. } => None,
        })
        .collect();
    let mut stored: HashMap<String, ItemRow> = tx
        .rows(&need)
        .await?
        .into_iter()
        .map(|r| (r.id.clone(), r))
        .collect();

    let mut writes = Vec::with_capacity(planned.len());
    for p in planned {
        let (row, change) = match p {
            Planned::Content {
                index,
                revision,
                status,
                change,
            } => (
                row_of(&collected.items[index], revision, status, &commit),
                change,
            ),
            Planned::StatusOnly {
                id,
                revision,
                status,
                change,
            } => {
                let mut row = stored.remove(&id).expect("计划里的条目在库里");
                row.revision = revision;
                row.status = status.as_str().to_owned();
                row.commit_id = commit.clone();
                (row, change)
            }
        };
        *report.counts.entry(change.as_str()).or_default() += 1;
        writes.push(ItemWrite { row, change });
    }
    tx.write(&writes).await?;
    tx.finish(&commit).await?;

    report.warnings = collected.warnings;
    report.warnings.extend(plan_warnings);
    report.took = started.elapsed();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tcms_core::{ItemId, Kind, Scope};

    fn item(id: &str, path: &str, body: &str, supersedes: Option<&str>) -> ParsedItem {
        ParsedItem {
            id: Some(id.parse().unwrap()),
            scope: Scope::Org,
            kind: Kind::Fact,
            title: "t".into(),
            body: body.into(),
            tags: vec![],
            owner: None,
            review_by: None,
            supersedes: supersedes.map(|s| s.parse::<ItemId>().unwrap()),
            path: path.into(),
            line: 1,
            meta_line: None,
            insert_at: 0,
        }
    }

    fn state(it: &ParsedItem, status: Status, revision: i32) -> ItemState {
        ItemState {
            id: it.id.as_ref().unwrap().to_string(),
            status: status.as_str().into(),
            revision,
            content_hash: it.content_hash(),
            path: it.path.clone(),
            supersedes: it.supersedes.as_ref().map(ToString::to_string),
        }
    }

    const A: &str = "ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA";
    const B: &str = "ctx_01JDX4M6Q8BBBBBBBBBBBBBBBB";
    const C: &str = "ctx_01JDX4M6Q8CCCCCCCCCCCCCCCC";

    fn changes(p: &[Planned]) -> Vec<(ChangeKind, Status, i32)> {
        p.iter()
            .map(|x| match x {
                Planned::Content {
                    change,
                    status,
                    revision,
                    ..
                }
                | Planned::StatusOnly {
                    change,
                    status,
                    revision,
                    ..
                } => (*change, *status, *revision),
            })
            .collect()
    }

    #[test]
    fn create_update_archive() {
        let a = item(A, "org/x.md", "一", None);
        let b = item(B, "org/x.md", "二", None);
        let c1 = Collected {
            full: true,
            items: vec![a.clone(), b.clone()],
            ..Default::default()
        };
        let (p, w) = plan(&[], &c1);
        assert!(w.is_empty());
        assert_eq!(
            changes(&p),
            [
                (ChangeKind::Created, Status::Active, 1),
                (ChangeKind::Created, Status::Active, 1)
            ]
        );

        // 同样的内容再来一遍：没有变更。
        let states = [state(&a, Status::Active, 1), state(&b, Status::Active, 1)];
        assert!(plan(&states, &c1).0.is_empty());

        // 改了 a、删了 b。
        let a2 = item(A, "org/x.md", "一改", None);
        let c2 = Collected {
            affected_paths: ["org/x.md".to_owned()].into(),
            items: vec![a2],
            ..Default::default()
        };
        assert_eq!(
            changes(&plan(&states, &c2).0),
            [
                (ChangeKind::Updated, Status::Active, 2),
                (ChangeKind::Archived, Status::Archived, 2)
            ]
        );
    }

    #[test]
    fn untouched_files_are_kept() {
        let a = item(A, "org/x.md", "一", None);
        let b = item(B, "org/y.md", "二", None);
        let states = [state(&a, Status::Active, 1), state(&b, Status::Active, 1)];
        let c = Collected {
            affected_paths: ["org/x.md".to_owned()].into(),
            items: vec![a],
            ..Default::default()
        };
        assert!(plan(&states, &c).0.is_empty());
    }

    #[test]
    fn supersedes_in_other_file_and_restore() {
        let a = item(A, "org/x.md", "旧决定", None);
        let b = item(B, "org/y.md", "新决定", Some(A));
        // b 在新文件里出现，取代 a；a 所在的文件没动。
        let states = [state(&a, Status::Active, 1)];
        let c = Collected {
            affected_paths: ["org/y.md".to_owned()].into(),
            items: vec![b.clone()],
            ..Default::default()
        };
        let p = plan(&states, &c).0;
        assert_eq!(
            changes(&p),
            [
                (ChangeKind::Superseded, Status::Superseded, 2),
                (ChangeKind::Created, Status::Active, 1)
            ]
        );
        assert!(matches!(&p[0], Planned::StatusOnly { id, .. } if id == A));

        // 改了 a 的文件，a 仍被取代（取代它的 b 在没动的文件里）。
        let a2 = item(A, "org/x.md", "旧决定，补了一句", None);
        let states = [
            state(&a, Status::Superseded, 2),
            state(&b, Status::Active, 1),
        ];
        let c = Collected {
            affected_paths: ["org/x.md".to_owned()].into(),
            items: vec![a2],
            ..Default::default()
        };
        assert_eq!(
            changes(&plan(&states, &c).0),
            [(ChangeKind::Updated, Status::Superseded, 3)]
        );

        // 删了 b：b 归档，a 恢复。
        let c = Collected {
            affected_paths: ["org/y.md".to_owned()].into(),
            items: vec![],
            ..Default::default()
        };
        assert_eq!(
            changes(&plan(&states, &c).0),
            [
                (ChangeKind::Restored, Status::Active, 3),
                (ChangeKind::Archived, Status::Archived, 2)
            ]
        );
    }

    #[test]
    fn moved_between_files_and_back_from_archive() {
        let a = item(A, "org/x.md", "一", None);
        let moved = item(A, "org/z.md", "一", None);
        let states = [state(&a, Status::Active, 1)];
        let c = Collected {
            affected_paths: ["org/x.md".to_owned(), "org/z.md".to_owned()].into(),
            items: vec![moved.clone()],
            ..Default::default()
        };
        assert_eq!(
            changes(&plan(&states, &c).0),
            [(ChangeKind::Updated, Status::Active, 2)]
        );

        let states = [state(&a, Status::Archived, 2)];
        let c = Collected {
            affected_paths: ["org/z.md".to_owned()].into(),
            items: vec![moved],
            ..Default::default()
        };
        assert_eq!(
            changes(&plan(&states, &c).0),
            [(ChangeKind::Restored, Status::Active, 3)]
        );
    }

    #[test]
    fn duplicate_id_in_untouched_file_wins() {
        let a = item(A, "org/x.md", "一", None);
        let dup = item(A, "org/y.md", "冒用", None);
        let c_item = item(C, "org/y.md", "三", None);
        let states = [state(&a, Status::Active, 1)];
        let c = Collected {
            affected_paths: ["org/y.md".to_owned()].into(),
            items: vec![dup, c_item],
            ..Default::default()
        };
        let (p, w) = plan(&states, &c);
        assert_eq!(changes(&p), [(ChangeKind::Created, Status::Active, 1)]);
        assert_eq!(w.len(), 1);
        assert!(w[0].message.contains("org/x.md"));
    }
}
