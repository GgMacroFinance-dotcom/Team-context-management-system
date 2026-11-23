//! 人批准、拒绝提议（`docs/architecture.md` 第 3 节）。
//!
//! 批准的步骤：拿到提交队列的锁（本进程里排队，跨进程用 PostgreSQL 的锁，一次只做一个提交）→
//! 配了远端时先从远端拉（[`crate::sync`]）→ 把索引带到 HEAD（人可能直接改过仓库）→ 看目标条目还在不在、版本号有没有变 → 改文件 → 提交
//! （要求 HEAD 没变）→ 索引（服务收到通知后换上）→ 记下接受了 → 配了远端时推上去。
//!
//! 做到一半断了（提交了但没记下）再批准一次时，认得出已经写进去了，直接记下，不会写两遍：
//! 新建、取代的条目 id 来自提议 id；修改的看内容是不是已经是提议的样子；归档的看是不是已经归档。

use tcms_core::edit::{Edit, EditError, ItemText, apply};
use tcms_core::{ItemId, Kind, Scope, normalize_body, normalize_title, parse_file, scope_of_path};
use tcms_git::{Author, ContextRepo, FileEdit, ObjectId, Remote, RemoteError};
use tcms_store::{Action, ItemRow, ProposalRow, Store};

use crate::propose::item_id_of;
use crate::sync::{RemoteReport, pull_before, push_after};

/// 批准时人可以先改再接受。
#[derive(Debug, Clone, Default)]
pub struct AcceptOptions {
    /// 审核的人（记进审计和提交信息）。
    pub actor: String,
    /// git 提交者。
    pub author: Author,
    /// 改了标题再接受。
    pub title: Option<String>,
    /// 改了正文再接受。
    pub body: Option<String>,
    /// 新建时写进哪个文件（仓库里的相对路径，第一层目录要和提议的范围一致）。默认 `<范围>/<类型的中文名>.md`。
    pub file: Option<String>,
    /// 条目在提议之后被人改过（版本号落后）时，写明看过的是第几版，照样接受（覆盖成提议的样子，或用上面改过的内容）。
    pub over_revision: Option<i32>,
    pub note: Option<String>,
    /// 上下文仓库的远端：提交之前先拉，提交以后推上去。`None` 是没配远端。
    pub remote: Option<Remote>,
}

/// 接受的结果。
#[derive(Debug, Clone)]
pub struct Accepted {
    pub proposal: ProposalRow,
    /// 写成的条目。
    pub item: String,
    pub commit: String,
    pub path: String,
    /// 之前已经写进去了（做到一半断过），这次只是记下。
    pub already_applied: bool,
    /// 配了远端时，拉和推的结果。
    pub remote: Option<RemoteReport>,
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("没有这条提议：{0}")]
    NotFound(String),
    #[error("提议 {id} 已经{status}，不能再处理")]
    NotOpen { id: String, status: String },
    #[error(
        "条目 {item} 在提议之后被改过：提议时是第 {base} 版，现在是第 {current} 版。看过现在的内容以后，加 --over {current} 再接受（可以用 --body-file 给出合并后的正文），或者拒绝"
    )]
    Stale {
        item: String,
        base: i32,
        current: i32,
    },
    #[error("条目 {0} 已经不在了（归档或被取代了），这条提议只能拒绝")]
    TargetGone(String),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Edit(#[from] EditError),
    #[error(transparent)]
    Git(#[from] tcms_git::Error),
    /// 和远端合并不了，要人合并（这时什么都没改）。
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error(transparent)]
    Index(#[from] tcms_index::Error),
    #[error(transparent)]
    Store(#[from] tcms_store::Error),
}

/// 新建时默认写进的文件：`role-macro/记忆.md`。
pub fn default_path(scope: &Scope, kind: Kind) -> String {
    format!("{}/{}.md", scope.dir(), kind.label())
}

fn bad<T>(msg: impl Into<String>) -> Result<T, WriteError> {
    Err(WriteError::Invalid(msg.into()))
}

/// 读 HEAD 里的一个文件，没有时返回 `None`。
pub(crate) fn read_at(
    repo: &ContextRepo,
    head: Option<ObjectId>,
    path: &str,
) -> Result<Option<String>, WriteError> {
    let Some(head) = head else { return Ok(None) };
    let Some(blob) = repo.files(head)?.get(path).copied() else {
        return Ok(None);
    };
    match String::from_utf8(repo.read_blob(blob)?) {
        Ok(s) => Ok(Some(s)),
        Err(_) => bad(format!("{path} 不是 UTF-8 文本，不能改")),
    }
}

/// 批准一条提议。
pub async fn accept(
    repo: &ContextRepo,
    store: &Store,
    id: &str,
    opts: &AcceptOptions,
) -> Result<Accepted, WriteError> {
    let Some(decision) = store.begin_decision(id).await? else {
        return Err(WriteError::NotFound(id.to_owned()));
    };
    let p = decision.proposal.clone();
    if p.status != "open" {
        return Err(WriteError::NotOpen {
            id: p.id,
            status: status_word(&p.status).to_owned(),
        });
    }
    let action: Action = p.action.parse().map_err(WriteError::Invalid)?;
    let kind: Kind = p.kind.parse().map_err(WriteError::Invalid)?;
    let scope: Scope = p.scope.parse().map_err(WriteError::Invalid)?;
    let result_id = item_id_of(&p);
    if opts.file.is_some() && matches!(action, Action::Update | Action::Archive) {
        return bad("--file 只用于新建和取代");
    }

    // 配了远端时先拉：人可能推过新的提交。合并不了就停下来，提议还是待审核。
    let mut report = pull_before(opts.remote.as_ref(), repo, &opts.author).await?;
    // 人可能直接改过仓库：先把索引带到 HEAD，后面的比对都照着 HEAD。
    tcms_index::reindex(repo, store, false).await?;
    let head = repo.head()?;

    let title = normalize_title(opts.title.as_deref().unwrap_or(&p.title));
    let body = normalize_body(opts.body.as_deref().unwrap_or(&p.body));
    if title.is_empty() {
        return bad("标题不能是空的");
    }

    let target: Option<ItemRow> = match &p.item_id {
        Some(item) if action.needs_item() => store.get_item(item).await?,
        _ => None,
    };
    let done = |row: &ItemRow| (row.path.clone(), row.commit_id.clone());

    // 已经写进去了吗。
    let already: Option<(String, String)> = match action {
        Action::Create | Action::Supersede => store
            .get_item(&result_id)
            .await?
            .filter(|r| r.status != "archived")
            .map(|r| done(&r)),
        Action::Update => target
            .as_ref()
            .filter(|r| {
                r.status == "active"
                    && r.revision != p.base_revision.unwrap_or(r.revision)
                    && r.title == title
                    && r.body == body
                    && r.kind == p.kind
                    && r.tags == p.tags
                    && r.review_by == p.review_by
            })
            .map(done),
        Action::Archive => target.as_ref().filter(|r| r.status == "archived").map(done),
    };
    if let Some((path, commit)) = already {
        let proposal = decision
            .accept(&opts.actor, opts.note.as_deref(), &result_id, &commit)
            .await?;
        // 上次可能是推之前断的。
        push_after(opts.remote.as_ref(), repo, &mut report).await;
        return Ok(Accepted {
            proposal,
            item: result_id,
            commit,
            path,
            already_applied: true,
            remote: report,
        });
    }

    // 目标条目还在、没被改过。
    if action.needs_item() {
        let item = p.item_id.clone().unwrap_or_default();
        let row = match &target {
            Some(r) if r.status == "active" => r,
            _ => return Err(WriteError::TargetGone(item)),
        };
        let base = p.base_revision.unwrap_or(row.revision);
        if row.revision != base && opts.over_revision != Some(row.revision) {
            return Err(WriteError::Stale {
                item,
                base,
                current: row.revision,
            });
        }
    }

    let new_text = |id: ItemId, owner: Option<String>, supersedes: Option<ItemId>| ItemText {
        id,
        kind,
        title: title.clone(),
        body: body.clone(),
        tags: p.tags.clone(),
        owner,
        review_by: p.review_by.clone(),
        supersedes,
    };
    let parse_id = |s: &str| s.parse::<ItemId>().map_err(WriteError::Invalid);
    let file_for_new = |opts_file: &Option<String>| -> Result<String, WriteError> {
        let path = opts_file
            .clone()
            .unwrap_or_else(|| default_path(&scope, kind));
        match scope_of_path(&path) {
            Some(s) if s == scope => Ok(path),
            Some(s) => bad(format!("{path} 在范围 {s} 里，这条提议的范围是 {scope}")),
            None => bad(format!(
                "{path} 不是条目文件（第一层目录要是范围，扩展名 .md）"
            )),
        }
    };
    let (path, edit) = match action {
        Action::Create => {
            let path = file_for_new(&opts.file)?;
            (
                path,
                Edit::Append(new_text(parse_id(&result_id)?, None, None)),
            )
        }
        Action::Update => {
            let row = target.as_ref().expect("上面确认过");
            let current = read_at(repo, head, &row.path)?.unwrap_or_default();
            let parsed = parse_file(&row.path, &current);
            let Some(old) = parsed
                .items
                .iter()
                .find(|it| it.id.as_ref().map(ItemId::as_str) == Some(row.id.as_str()))
            else {
                return Err(WriteError::TargetGone(row.id.clone()));
            };
            let text = new_text(
                parse_id(&row.id)?,
                old.owner.clone(),
                old.supersedes.clone(),
            );
            (row.path.clone(), Edit::Replace(text))
        }
        Action::Supersede => {
            let row = target.as_ref().expect("上面确认过");
            let text = new_text(parse_id(&result_id)?, None, Some(parse_id(&row.id)?));
            let same_scope = row.scope == p.scope;
            match &opts.file {
                None if same_scope => (
                    row.path.clone(),
                    Edit::InsertAfter {
                        after: parse_id(&row.id)?,
                        item: text,
                    },
                ),
                file => (file_for_new(file)?, Edit::Append(text)),
            }
        }
        Action::Archive => {
            let row = target.as_ref().expect("上面确认过");
            (row.path.clone(), Edit::Remove(parse_id(&row.id)?))
        }
    };
    let current = read_at(repo, head, &path)?;
    let content = apply(&path, current.as_deref(), &edit)?;
    let message = commit_message(&p, action, &title, &opts.actor);
    let commit = repo.commit_on(
        head,
        &[FileEdit {
            path: path.clone(),
            content: content.map(String::into_bytes),
        }],
        &message,
        &opts.author,
    )?;
    tcms_index::reindex(repo, store, false).await?;
    let commit = commit.to_string();
    let proposal = decision
        .accept(&opts.actor, opts.note.as_deref(), &result_id, &commit)
        .await?;
    // 接受已经记下了，推不上去只记在结果里，下次同步再推。
    push_after(opts.remote.as_ref(), repo, &mut report).await;
    Ok(Accepted {
        proposal,
        item: result_id,
        commit,
        path,
        already_applied: false,
        remote: report,
    })
}

/// 拒绝一条提议。
pub async fn reject(
    store: &Store,
    id: &str,
    actor: &str,
    note: Option<&str>,
) -> Result<(), WriteError> {
    let Some(decision) = store.begin_decision(id).await? else {
        return Err(WriteError::NotFound(id.to_owned()));
    };
    if decision.proposal.status != "open" {
        return Err(WriteError::NotOpen {
            id: decision.proposal.id.clone(),
            status: status_word(&decision.proposal.status).to_owned(),
        });
    }
    decision.reject(actor, note).await?;
    Ok(())
}

fn status_word(status: &str) -> &str {
    match status {
        "accepted" => "接受",
        "rejected" => "拒绝",
        other => other,
    }
}

fn commit_message(p: &ProposalRow, action: Action, title: &str, actor: &str) -> String {
    let verb = match action {
        Action::Create => "新建",
        Action::Update => "修改",
        Action::Supersede => "取代",
        Action::Archive => "归档",
    };
    let first = format!("{verb}：{title}");
    let first: String = first.chars().take(72).collect();
    format!(
        "{first}\n\n{}\n\nProposal: {}\nProposed-by: {} ({})\nAccepted-by: {actor}\n",
        p.reason, p.id, p.proposer_name, p.proposer_token
    )
}
