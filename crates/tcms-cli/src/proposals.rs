//! `tcms proposals`、`tcms accept`、`tcms reject`：人处理 agent 的提议（`docs/spec.md` 第 5 节、6.2）。
//! `tcms promote`：人发起晋升，做成一条提议。

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde_json::json;
use tcms_core::Scope;
use tcms_git::{ContextRepo, Remote};
use tcms_store::{Identity, ItemRow, ProposalRow, Store};
use tcms_write::{AcceptOptions, Conflict, DiffLine, proposal_diff};

use crate::{actor, author};

#[derive(Clone, Copy, ValueEnum)]
pub enum Status {
    Open,
    Accepted,
    Rejected,
    All,
}

impl Status {
    fn filter(self) -> Option<&'static str> {
        match self {
            Status::Open => Some("open"),
            Status::Accepted => Some("accepted"),
            Status::Rejected => Some("rejected"),
            Status::All => None,
        }
    }
}

fn status_word(s: &str) -> &str {
    match s {
        "open" => "待审核",
        "accepted" => "已接受",
        "rejected" => "已拒绝",
        other => other,
    }
}

fn action_word(a: &str) -> &str {
    match a {
        "create" => "新建",
        "update" => "修改",
        "supersede" => "取代",
        "archive" => "归档",
        other => other,
    }
}

fn conflicts(p: &ProposalRow) -> Vec<Conflict> {
    serde_json::from_value(p.conflicts.clone()).unwrap_or_default()
}

/// 列出提议。
pub async fn list(store: &Store, status: Status, json: bool) -> Result<()> {
    let rows = store.list_proposals(status.filter()).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!(
            "{}",
            match status {
                Status::Open => "没有待审核的提议。",
                _ => "没有提议。",
            }
        );
        return Ok(());
    }
    for p in &rows {
        let target = p
            .item_id
            .as_deref()
            .map(|i| format!(" {i}"))
            .unwrap_or_default();
        let n = conflicts(p).len();
        println!(
            "{}  {}  {}{}  {} / {}  {}",
            p.id,
            status_word(&p.status),
            action_word(&p.action),
            target,
            p.scope,
            p.kind,
            p.title
        );
        println!(
            "  {}（{}）提于 {} UTC{}",
            p.proposer_name,
            p.proposer_token,
            p.created_at,
            if n > 0 {
                format!("；相近的条目 {n} 条")
            } else {
                String::new()
            }
        );
    }
    if matches!(status, Status::Open) {
        println!("看一条：tcms proposals <id>；处理：tcms accept <id> 或 tcms reject <id>");
    }
    Ok(())
}

/// 看一条提议：内容、理由、证据、相近的条目，和批准以后差在哪里。
pub async fn show(store: &Store, id: &str, json: bool) -> Result<()> {
    let Some(p) = store.get_proposal(id).await? else {
        bail!("没有这条提议：{id}");
    };
    let current: Option<ItemRow> = match &p.item_id {
        Some(item) => store.get_item(item).await?,
        None => None,
    };
    let diff = proposal_diff(&p, current.as_ref());
    let stale = match (&current, p.base_revision) {
        (Some(c), Some(base)) => c.revision != base,
        _ => false,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "proposal": p,
                "current": current,
                "stale": stale,
                "diff": diff,
            }))?
        );
        return Ok(());
    }
    println!(
        "{}  {}  {}  {} / {}",
        p.id,
        status_word(&p.status),
        action_word(&p.action),
        p.scope,
        p.kind
    );
    println!(
        "提议人：{}（{}，岗位 {}），{} UTC",
        p.proposer_name, p.proposer_token, p.proposer_role, p.created_at
    );
    println!("理由：{}", p.reason);
    if !p.evidence.is_empty() {
        println!("证据：");
        for e in &p.evidence {
            println!("  - {e}");
        }
    }
    if p.redacted > 0 {
        println!("脱敏：抹掉了 {} 处像密钥的内容", p.redacted);
    }
    if let Some(item) = &p.item_id {
        match &current {
            Some(c) if c.status != "active" => println!(
                "目标：{item} 现在是{}，这条提议只能拒绝",
                if c.status == "archived" {
                    "已归档"
                } else {
                    "被取代了"
                }
            ),
            Some(c) if stale => println!(
                "目标：{item}，提议时是第 {} 版，现在是第 {} 版（提议之后被人改过，下面和现在的比；接受时要加 --over {}）",
                p.base_revision.unwrap_or_default(),
                c.revision,
                c.revision
            ),
            Some(c) => println!("目标：{item}，第 {} 版，在 {}", c.revision, c.path),
            None => println!("目标：{item} 已经不在了"),
        }
    }
    let near = conflicts(&p);
    if !near.is_empty() {
        println!("相近的条目（可能重复或矛盾）：");
        for c in &near {
            println!(
                "  {} 第 {} 版  {}（相近 {}%）",
                c.id, c.revision, c.title, c.similarity
            );
        }
    }
    match p.status.as_str() {
        "open" => {}
        _ => println!(
            "处理：{} 于 {} UTC{}{}",
            p.decided_by.as_deref().unwrap_or("-"),
            p.decided_at.as_deref().unwrap_or("-"),
            p.decision_note
                .as_deref()
                .map(|n| format!("，备注：{n}"))
                .unwrap_or_default(),
            p.result_commit
                .as_deref()
                .map(|c| format!("，提交 {c}"))
                .unwrap_or_default()
        ),
    }
    println!();
    println!(
        "{}",
        match p.action.as_str() {
            "create" => "批准以后新加这一条：",
            "archive" => "批准以后归档这一条：",
            _ => "批准以后（- 现在的，+ 提议的）：",
        }
    );
    for line in &diff {
        match line {
            DiffLine::Same(l) => println!("  {l}"),
            DiffLine::Removed(l) => println!("- {l}"),
            DiffLine::Added(l) => println!("+ {l}"),
        }
    }
    Ok(())
}

pub struct AcceptArgs {
    pub id: String,
    pub title: Option<String>,
    pub body_file: Option<PathBuf>,
    pub file: Option<String>,
    pub over: Option<i32>,
    pub note: Option<String>,
    pub json: bool,
}

/// 批准：写进 git，提交，索引。
pub async fn accept(
    repo: &ContextRepo,
    store: &Store,
    remote: Option<Remote>,
    a: AcceptArgs,
) -> Result<()> {
    let body = match &a.body_file {
        None => None,
        Some(p) if p.as_os_str() == "-" => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .context("从标准输入读正文")?;
            Some(s)
        }
        Some(p) => Some(
            std::fs::read_to_string(p).with_context(|| format!("读正文文件 {}", p.display()))?,
        ),
    };
    let opts = AcceptOptions {
        actor: actor(),
        author: author(),
        title: a.title,
        body,
        file: a.file,
        over_revision: a.over,
        note: a.note,
        remote,
    };
    let done = tcms_write::accept(repo, store, &a.id, &opts).await?;
    if a.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "proposal": done.proposal,
                "item": done.item,
                "commit": done.commit,
                "path": done.path,
                "already_applied": done.already_applied,
                "remote": done.remote,
            }))?
        );
    } else if done.already_applied {
        println!(
            "{} 之前已经写进去了（{}，提交 {}），这次只记下接受。",
            a.id, done.path, done.commit
        );
    } else {
        println!(
            "已接受 {}：{} {}，写进 {}，提交 {}。",
            a.id,
            action_word(&done.proposal.action),
            done.item,
            done.path,
            done.commit
        );
        println!("索引已更新；运行中的 tcms serve 收到通知后换上新索引。");
    }
    if !a.json
        && let Some(r) = &done.remote
    {
        crate::print_remote(r);
    }
    Ok(())
}

/// 晋升：做成一条提议，打印它的 id。`to` 不写时提到近的那一级。
pub async fn promote(
    repo: &ContextRepo,
    store: &Store,
    id: &str,
    to: Option<&str>,
    reason: &str,
    json: bool,
) -> Result<()> {
    let index = crate::read::load(store).await?;
    let teams = match index.corpus().commit.as_deref() {
        Some(c) => tcms_write::read_teams(repo, c).map_err(anyhow::Error::msg)?,
        None => None,
    };
    let to: Scope = match to {
        Some(t) => t.parse().map_err(anyhow::Error::msg)?,
        None => {
            let Some(item) = index.corpus().get(id) else {
                bail!("没有这一条，或者它已经不是生效的了：{id}");
            };
            match tcms_write::promotion_targets(&item.scope, teams.as_ref()).first() {
                Some(s) => s.clone(),
                None => bail!("{id} 已经是全公司的了，没有更大的范围"),
            }
        }
    };
    // 岗位空着：是人提的，不是哪个岗位的 agent。
    let by = Identity {
        token_id: "cli".to_owned(),
        name: actor(),
        role: String::new(),
        team: None,
        projects: Vec::new(),
    };
    let p = tcms_write::promote(store, &index, id, &to, reason, teams.as_ref(), &by).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&p)?);
    } else if p.duplicate {
        println!("一样的晋升已经有一条在等审核：{}。", p.proposal_id);
    } else {
        println!(
            "已做成晋升提议 {}：{id} 提到 {to}，批准以后写成 {}。用 tcms accept {} 批准，或在审核页处理。",
            p.proposal_id, p.result_item_id, p.proposal_id
        );
    }
    Ok(())
}

/// 拒绝。
pub async fn reject(store: &Store, id: &str, note: Option<&str>) -> Result<()> {
    tcms_write::reject(store, id, &actor(), note).await?;
    println!("已拒绝 {id}。");
    Ok(())
}

/// `tcms suggest-promotions`：系统找的晋升候选（`docs/spec.md` 第 5 节）。只列出来，不提议。
pub async fn suggest_promotions(repo: &ContextRepo, store: &Store, json: bool) -> Result<()> {
    let index = crate::read::load(store).await?;
    let corpus = index.corpus();
    let teams = match corpus.commit.as_deref() {
        Some(c) => tcms_write::read_teams(repo, c).map_err(anyhow::Error::msg)?,
        None => None,
    };
    let found = tcms_write::promotion_candidates(corpus, teams.as_ref());
    if json {
        println!("{}", serde_json::to_string_pretty(&found)?);
        return Ok(());
    }
    if found.is_empty() {
        println!("现在没有可以晋升的条目。");
        return Ok(());
    }
    println!("{} 组可以晋升的条目（几个范围都写了相近的）：", found.len());
    for c in &found {
        println!();
        println!("建议提到 {}（{}）：", c.to, c.kind);
        for it in &c.items {
            println!("  {}  {}  {}", it.id, it.scope, it.title);
        }
    }
    println!();
    println!("挑写得最好的一条：tcms promote <id> --to <范围> --reason \"...\"，批准以后才生效。");
    Ok(())
}
