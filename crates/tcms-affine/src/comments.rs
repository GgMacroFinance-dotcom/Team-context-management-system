//! 提议显示成 AFFiNE 评论（`docs/affine-sync.md` 5.5、7.6）。
//!
//! 看一遍（[`sync_comments`]）做两件事：
//! - 还没处理、目标条目（新建时是目标文件）在接上的文档里、还没发过评论的提议：在那篇文档上发一条评论，
//!   写谁提议做什么、理由的前 [`REASON_CHARS`] 个字、审核页的链接。不放提议的正文和证据（证据里可能有对话链接）。
//! - 发过、还没关的评论：取那篇文档上的评论，记下人在 AFFiNE 里的回复（审核页里列出来）；提议批准了、拒绝了的，
//!   回复一句结果，标成已解决；评论在 AFFiNE 里被删了的不再管。
//!
//! 评论不挂在文档里的文字上（不改文档），在网页版右边栏的“评论”里看。一条一条发、回复时出的错记下来，别的照做，
//! 下次再试。

use std::collections::{BTreeMap, HashMap, HashSet};

use affine_sync_client::{Comment, Span, comment_spans, comment_text};
use serde::Serialize;
use serde_json::{Value, json};
use tcms_core::{Kind, Scope};
use tcms_store::{AffineBase, AffineComment, AffineDoc, ProposalRow, Store};
use tcms_write::default_path;

use crate::bridge::Error;
use crate::server::Affine;

/// 理由在评论里最多放几个字。
pub const REASON_CHARS: usize = 200;

/// 看一遍评论做了什么。都是提议 id。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CommentReport {
    /// 新发了评论的。
    pub posted: Vec<String>,
    /// 回复了结果、标成已解决的。
    pub closed: Vec<String>,
    /// 评论在 AFFiNE 里被删了的。
    pub gone: Vec<String>,
    /// AFFiNE 里的回复有变化的。
    pub replies: Vec<String>,
    /// 出的错（这几条下次再试）。
    pub errors: Vec<String>,
}

impl CommentReport {
    /// 什么都没做、也没出错。
    pub fn is_empty(&self) -> bool {
        self.posted.is_empty()
            && self.closed.is_empty()
            && self.gone.is_empty()
            && self.replies.is_empty()
            && self.errors.is_empty()
    }
}

/// 看一遍提议和评论（上面模块说明的两件事）。`review_url` 是审核页对外的地址（`TCMS_PUBLIC_URL`，比如
/// `https://ctx.example.com`），没有时评论里写提议 id。
pub async fn sync_comments(
    store: &Store,
    affine: &mut impl Affine,
    workspace: &str,
    review_url: Option<&str>,
) -> Result<CommentReport, Error> {
    let mut report = CommentReport::default();
    let docs: Vec<AffineDoc> = store
        .affine_docs()
        .await?
        .into_iter()
        .filter(|d| d.workspace_id == workspace)
        .collect();
    let by_path: HashMap<&str, &AffineDoc> = docs.iter().map(|d| (d.path.as_str(), d)).collect();
    let comments = store.affine_comments(false).await?;
    let commented: HashSet<&str> = comments.iter().map(|c| c.proposal_id.as_str()).collect();

    // 新的提议：发评论。底稿只取这几条提议针对的条目的（1 万条时全部取一遍要二十几毫秒）。
    let new: Vec<ProposalRow> = store
        .list_proposals(Some("open"))
        .await?
        .into_iter()
        .filter(|p| !commented.contains(p.id.as_str()))
        .collect();
    let items: Vec<String> = new.iter().filter_map(|p| p.item_id.clone()).collect();
    let bases = store.affine_bases_of(&items).await?;
    let by_item: HashMap<&str, &AffineBase> =
        bases.iter().map(|b| (b.item_id.as_str(), b)).collect();
    for p in new {
        let Some(t) = target(&p, &by_item) else {
            continue;
        };
        let Some(doc) = by_path.get(t.path.as_str()) else {
            continue;
        };
        let content = proposal_comment(&p, &t, &doc.scope, review_url)?;
        match affine
            .create_comment(workspace, &doc.doc_id, doc_title(&doc.path), content)
            .await
        {
            Ok(comment_id) => {
                store
                    .add_affine_comment(&AffineComment {
                        proposal_id: p.id.clone(),
                        workspace_id: workspace.to_owned(),
                        doc_id: doc.doc_id.clone(),
                        path: doc.path.clone(),
                        comment_id,
                        closed: false,
                        replies: json!([]),
                        created_at: String::new(),
                    })
                    .await?;
                report.posted.push(p.id);
            }
            Err(e) => report
                .errors
                .push(format!("给提议 {} 发评论（{}）：{e}", p.id, doc.path)),
        }
    }

    // 发过、还没关的：这几篇文档的评论一次取齐（连真 AFFiNE 时一次查询问几篇）。一次取出错时一篇一篇取，
    // 一篇的错不耽误别的。
    let mut open: BTreeMap<&str, Vec<&AffineComment>> = BTreeMap::new();
    for c in comments
        .iter()
        .filter(|c| !c.closed && c.workspace_id == workspace)
    {
        open.entry(c.doc_id.as_str()).or_default().push(c);
    }
    let doc_ids: Vec<String> = open.keys().map(|d| (*d).to_owned()).collect();
    let mut all = if doc_ids.is_empty() {
        HashMap::new()
    } else {
        affine
            .comments_of(workspace, &doc_ids)
            .await
            .unwrap_or_default()
    };
    for (doc_id, rows) in open {
        let listed = match all.remove(doc_id) {
            Some(l) => l,
            None => match affine.comments(workspace, doc_id).await {
                Ok(l) => l,
                Err(e) => {
                    report
                        .errors
                        .push(format!("取 {} 上的评论：{e}", rows[0].path));
                    continue;
                }
            },
        };
        let by_id: HashMap<&str, &Comment> = listed.iter().map(|c| (c.id.as_str(), c)).collect();
        for row in rows {
            let Some(found) = by_id.get(row.comment_id.as_str()) else {
                store.close_affine_comment(&row.proposal_id).await?;
                report.gone.push(row.proposal_id.clone());
                continue;
            };
            let Some(p) = store.get_proposal(&row.proposal_id).await? else {
                continue;
            };
            let said = (p.status != "open").then(|| outcome(&p));
            let replies = replies(found, said.as_deref());
            if replies != row.replies {
                store
                    .set_affine_comment_replies(&row.proposal_id, &replies)
                    .await?;
                report.replies.push(row.proposal_id.clone());
            }
            let Some(said) = said else {
                continue;
            };
            // 上次回复了、没标成已解决的，不再回复一遍。
            let replied = found
                .replies
                .iter()
                .any(|r| comment_text(&r.content) == said);
            let done = async {
                if !replied {
                    let content = comment_spans(&[vec![Span::Text(&said)]], None)?;
                    affine
                        .reply(&row.comment_id, doc_title(&row.path), content)
                        .await?;
                }
                affine.resolve_comment(&row.comment_id).await
            };
            match done.await {
                Ok(()) => {
                    store.close_affine_comment(&row.proposal_id).await?;
                    report.closed.push(row.proposal_id.clone());
                }
                Err(e) => report.errors.push(format!(
                    "回复提议 {} 的评论（{}）：{e}",
                    row.proposal_id, row.path
                )),
            }
        }
    }
    Ok(report)
}

/// 评论发在哪里：文件，和针对的那一条（新建时没有）。
struct Target {
    path: String,
    /// 针对的那一条现在（上次同步完）的标题。
    item_title: Option<String>,
}

fn target(p: &ProposalRow, by_item: &HashMap<&str, &AffineBase>) -> Option<Target> {
    match &p.item_id {
        Some(item) => {
            let b = by_item.get(item.as_str())?;
            Some(Target {
                path: b.path.clone(),
                item_title: Some(b.title.clone()),
            })
        }
        None => {
            let scope: Scope = p.scope.parse().ok()?;
            let kind: Kind = p.kind.parse().ok()?;
            Some(Target {
                path: default_path(&scope, kind),
                item_title: None,
            })
        }
    }
}

/// 文档的标题（接上时用的文件名）。
fn doc_title(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".md").unwrap_or(name)
}

/// 审计里、提议上记的“谁”是“名字（令牌 id）”，评论里只写名字。
fn person(actor: &str) -> &str {
    actor.split('（').next().unwrap_or(actor).trim()
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// 评论的内容：谁提议做什么、理由、去哪里处理。
fn proposal_comment(
    p: &ProposalRow,
    t: &Target,
    doc_scope: &str,
    review_url: Option<&str>,
) -> Result<Value, Error> {
    let who = if p.proposer_role.is_empty() {
        p.proposer_name.clone()
    } else {
        format!("{}（岗位 {}）", p.proposer_name, p.proposer_role)
    };
    let current = t.item_title.as_deref().unwrap_or(&p.title);
    let what = match p.action.as_str() {
        "create" => format!("在这篇里新加一条「{}」", p.title),
        "update" if current != p.title => format!("修改「{current}」，标题改成「{}」", p.title),
        "update" => format!("修改「{current}」"),
        "supersede" if p.scope != doc_scope => format!("把「{current}」提到 {}", p.scope),
        "supersede" => format!("用「{}」取代「{current}」", p.title),
        "archive" => format!("归档「{current}」"),
        other => format!("{other}「{}」", p.title),
    };
    let first = format!("{who}提议{what}。");
    let reason = format!("理由：{}", truncate(&p.reason, REASON_CHARS));
    let url =
        review_url.map(|base| format!("{}/review#/proposals/{}", base.trim_end_matches('/'), p.id));
    let fallback = format!("批准、拒绝在审核页的“待审核的提议”里（{}）。", p.id);
    let last = match &url {
        Some(url) => vec![
            Span::Text("批准、拒绝在审核页："),
            Span::Link {
                text: "打开这条提议",
                url,
            },
            Span::Text("。在这里回复的，审核页里看得到。"),
        ],
        None => vec![
            Span::Text(&fallback),
            Span::Text("在这里回复的，审核页里看得到。"),
        ],
    };
    Ok(comment_spans(
        &[vec![Span::Text(&first)], vec![Span::Text(&reason)], last],
        t.item_title.as_deref(),
    )?)
}

/// 处理结果的那句回复。
fn outcome(p: &ProposalRow) -> String {
    let who = p.decided_by.as_deref().map(person).unwrap_or("审核的人");
    let note = p
        .decision_note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());
    match p.status.as_str() {
        "accepted" => {
            let commit = p
                .result_commit
                .as_deref()
                .map(|c| format!("，提交 {}", &c[..c.len().min(12)]))
                .unwrap_or_default();
            let note = note.map(|n| format!("备注：{n}。")).unwrap_or_default();
            format!("已接受（{who}），写进了上下文仓库{commit}。{note}")
        }
        "rejected" => match note {
            Some(n) => format!("已拒绝（{who}）：{n}"),
            None => format!("已拒绝（{who}）。"),
        },
        other => format!("提议现在是 {other}（{who}）。"),
    }
}

/// 人在 AFFiNE 里的回复（不算自己回复的结果）：`[{id, user, text, at}]`。
fn replies(c: &Comment, said: Option<&str>) -> Value {
    Value::Array(
        c.replies
            .iter()
            .map(|r| (r, comment_text(&r.content)))
            .filter(|(_, text)| Some(text.as_str()) != said)
            .map(|(r, text)| {
                json!({"id": r.id, "user": r.user_name, "text": text, "at": r.created_at})
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_and_reasons() {
        assert_eq!(doc_title("role-macro/数据源.md"), "数据源");
        assert_eq!(person("王审核（tok_01ABC）"), "王审核");
        assert_eq!(person("cli"), "cli");
        assert_eq!(truncate("  一二三  ", 3), "一二三");
        assert_eq!(truncate("一二三四", 3), "一二三…");
    }
}
