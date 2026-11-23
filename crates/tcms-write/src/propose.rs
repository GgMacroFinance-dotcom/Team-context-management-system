//! `context_propose`：agent 的提议（`docs/spec.md` 第 5 节、6.1）。
//!
//! 存进 PostgreSQL 的是提议里那一条完整的样子：修改时没写的字段从当时的条目补上，归档时照抄当时的条目，
//! 审核的人看到的就是批准以后会写进去的东西。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tcms_core::edit::{Edit, ItemText, apply};
use tcms_core::{ItemId, Kind, Scope, normalize_body, normalize_title, redact, valid_date};
use tcms_search::{CorpusItem, SearchIndex, Visibility};
use tcms_store::{ALL_PROJECTS, Action, Identity, NewProposal, ProposalRow, Proposed, Store};

use crate::conflicts::{Conflict, find_conflicts};

/// 一个 agent（令牌）最多攒多少条没处理的提议。
pub const MAX_OPEN_PER_AGENT: i64 = 20;
const MAX_TITLE_CHARS: usize = 200;
const MAX_BODY_CHARS: usize = 20_000;
const MAX_REASON_CHARS: usize = 2_000;
const MAX_EVIDENCE: usize = 10;
const MAX_EVIDENCE_CHARS: usize = 500;
const MAX_TAGS: usize = 20;
const MAX_TAG_CHARS: usize = 50;

/// `context_propose` 的参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposeArgs {
    pub action: String,
    pub item_id: Option<String>,
    pub draft: Option<Draft>,
    pub reason: String,
    #[serde(default)]
    pub evidence: Vec<String>,
}

/// 提议里的那一条。修改时只写要改的字段。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub title: Option<String>,
    pub body: Option<String>,
    pub kind: Option<String>,
    pub tags: Option<Vec<String>>,
    pub scope: Option<String>,
    pub review_by: Option<String>,
}

/// 提议的结果，回给 agent。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Proposal {
    pub proposal_id: String,
    pub status: String,
    /// 一样的内容已经有人提过、还没处理，这次没有新建，返回的是那一条。
    pub duplicate: bool,
    pub action: String,
    /// 修改、取代、归档的是哪一条。
    pub item_id: Option<String>,
    /// 批准以后写成的那一条的 id（新建、取代时是新的 id）。
    pub result_item_id: String,
    pub scope: String,
    pub kind: String,
    pub title: String,
    /// 同范围、同类型、标题或标签相近的条目。
    pub conflicts: Vec<Conflict>,
    /// 脱敏抹掉了几处。
    pub redacted: i32,
    pub note: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ProposeError {
    /// 参数或内容不对，说给 agent 听。
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Store(#[from] tcms_store::Error),
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, ProposeError> {
    Err(ProposeError::Invalid(msg.into()))
}

/// 读单条、问变化、检索、提议时能看的范围：`org`、团队、岗位，加上令牌允许的项目。
pub fn reach(who: &Identity) -> Visibility {
    let mut scopes = vec![Scope::Org, Scope::Role(who.role.clone())];
    if let Some(t) = &who.team {
        scopes.push(Scope::Team(t.clone()));
    }
    scopes.extend(
        who.projects
            .iter()
            .filter(|p| p.as_str() != ALL_PROJECTS)
            .map(|p| Scope::Project(p.clone())),
    );
    scopes.sort();
    if who.all_projects() {
        Visibility::ScopesAndAllProjects(scopes)
    } else {
        Visibility::Scopes(scopes)
    }
}

/// 提议批准以后写成的那一条的 id：修改、归档是原来那条；新建、取代用提议 id 里的那串 ULID，
/// 批准做到一半断了重来时认得出已经写进去了。
pub fn item_id_of(p: &ProposalRow) -> String {
    match (p.action.as_str(), &p.item_id) {
        ("update" | "archive", Some(id)) => id.clone(),
        _ => format!(
            "{}{}",
            ItemId::PREFIX,
            p.id.strip_prefix("prop_").unwrap_or(&p.id)
        ),
    }
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

const RULE_NOTE: &str =
    "规则只能人来改。可以提一条 decision 或 memory，写明建议怎么改、引用规则的 id";

/// 提一条。`index` 是服务手上的那一份（找目标条目、找矛盾用）。
pub async fn propose(
    store: &Store,
    index: &SearchIndex,
    who: &Identity,
    args: ProposeArgs,
) -> Result<Proposal, ProposeError> {
    let action: Action = match args.action.parse() {
        Ok(a) => a,
        Err(e) => return invalid(e),
    };
    let reason = args.reason.trim().to_owned();
    if reason.is_empty() {
        return invalid("reason 不能是空的：写明为什么要这样改，审核的人要看");
    }
    if chars(&reason) > MAX_REASON_CHARS {
        return invalid(format!("reason 太长了，最多 {MAX_REASON_CHARS} 字"));
    }
    if args.evidence.len() > MAX_EVIDENCE
        || args.evidence.iter().any(|e| chars(e) > MAX_EVIDENCE_CHARS)
    {
        return invalid(format!(
            "evidence 最多 {MAX_EVIDENCE} 条，每条最多 {MAX_EVIDENCE_CHARS} 字"
        ));
    }
    let visibility = reach(who);

    // 目标条目：看不到的和不存在的回答一样。
    let target: Option<&CorpusItem> = match (action.needs_item(), &args.item_id) {
        (false, Some(_)) => return invalid("新建（create）不用写 item_id"),
        (false, None) => None,
        (true, None) => return invalid(format!("{action} 要写 item_id：改哪一条")),
        (true, Some(id)) => match index.corpus().get(id) {
            Some(it) if visibility.allows(&it.scope) => Some(it),
            _ => return invalid(format!("没有这一条：{id}")),
        },
    };
    if target.is_some_and(|t| t.kind == Kind::Rule) {
        return invalid(RULE_NOTE);
    }
    let draft = args.draft.unwrap_or_default();
    if action == Action::Archive
        && (draft.title.is_some()
            || draft.body.is_some()
            || draft.kind.is_some()
            || draft.tags.is_some()
            || draft.scope.is_some()
            || draft.review_by.is_some())
    {
        return invalid("归档（archive）不用写 draft，理由写在 reason 里");
    }

    // 范围。
    let scope = match (&draft.scope, target) {
        (Some(s), _) => match s.parse::<Scope>() {
            Ok(s) => s,
            Err(e) => return invalid(e),
        },
        (None, Some(t)) => t.scope.clone(),
        (None, None) => Scope::Role(who.role.clone()),
    };
    if let Some(t) = target
        && matches!(action, Action::Update | Action::Archive)
        && t.scope != scope
    {
        return invalid("修改不能换范围；要换范围，用 supersede 在新范围里写一条取代它");
    }
    let writable = match &scope {
        Scope::Org => true,
        Scope::Team(t) => who.team.as_deref() == Some(t.as_str()),
        Scope::Role(r) => r == &who.role,
        Scope::Project(p) => who.may_use_project(p),
    };
    if !writable {
        return invalid(format!(
            "不能往 {scope} 提：这个令牌能提的范围是 org、{}role:{}，和令牌允许的项目",
            who.team
                .as_deref()
                .map(|t| format!("team:{t}、"))
                .unwrap_or_default(),
            who.role
        ));
    }

    // 类型。
    let kind = match (&draft.kind, target, action) {
        (Some(k), _, _) => match k.parse::<Kind>() {
            Ok(k) => k,
            Err(e) => return invalid(e),
        },
        (None, Some(t), _) => t.kind,
        (None, None, _) => Kind::Memory,
    };
    if kind == Kind::Rule {
        return invalid(RULE_NOTE);
    }

    // 标题、正文、标签、复核日期。
    let title = normalize_title(
        draft
            .title
            .as_deref()
            .or(target.map(|t| t.title.as_str()))
            .unwrap_or(""),
    );
    let body = match (&draft.body, target, action) {
        (Some(b), _, _) => normalize_body(b),
        (None, Some(_), Action::Supersede) => {
            return invalid("取代（supersede）要写新的正文 draft.body");
        }
        (None, Some(t), _) => t.body.clone(),
        (None, None, _) => String::new(),
    };
    if title.is_empty() {
        return invalid("draft.title 不能是空的");
    }
    if matches!(action, Action::Create | Action::Supersede) && body.is_empty() {
        return invalid("draft.body 不能是空的");
    }
    if chars(&title) > MAX_TITLE_CHARS {
        return invalid(format!("标题太长了，最多 {MAX_TITLE_CHARS} 字"));
    }
    if chars(&body) > MAX_BODY_CHARS {
        return invalid(format!(
            "正文太长了，最多 {MAX_BODY_CHARS} 字；长资料拆成几条，或者用 reference 放摘要加链接"
        ));
    }
    let mut tags: Vec<String> = match (&draft.tags, target) {
        (Some(t), _) => t.iter().map(|t| t.trim().to_owned()).collect(),
        (None, Some(t)) => t.tags.clone(),
        (None, None) => Vec::new(),
    };
    tags.retain(|t| !t.is_empty());
    let mut seen = std::collections::HashSet::new();
    tags.retain(|t| seen.insert(t.clone()));
    if tags.len() > MAX_TAGS
        || tags
            .iter()
            .any(|t| chars(t) > MAX_TAG_CHARS || t.contains([',', '\n', '"']))
    {
        return invalid(format!(
            "标签最多 {MAX_TAGS} 个，每个最多 {MAX_TAG_CHARS} 字，不能有逗号、换行、引号"
        ));
    }
    let review_by = match (&draft.review_by, target) {
        (Some(d), _) if d.trim().is_empty() => None,
        (Some(d), _) if valid_date(d.trim()) => Some(d.trim().to_owned()),
        (Some(d), _) => return invalid(format!("review_by 不是日期（YYYY-MM-DD）：{d}")),
        (None, Some(t)) => t.review_by.clone(),
        (None, None) => None,
    };

    // 脱敏。
    let mut redacted = 0;
    let mut clean = |s: &str| {
        let (out, n) = redact(s);
        redacted += n;
        out
    };
    let title = clean(&title);
    let body = clean(&body);
    let reason = clean(&reason);
    let evidence: Vec<String> = args.evidence.iter().map(|e| clean(e.trim())).collect();
    let tags: Vec<String> = tags.iter().map(|t| clean(t)).collect();

    if action == Action::Update
        && let Some(t) = target
        && t.title == title
        && t.body == body
        && t.kind == kind
        && t.tags == tags
        && t.review_by == review_by
    {
        return invalid("和现在的一样，没有要改的");
    }

    // 写得进 Markdown、读得回来（正文里不能有一级、二级标题之类）。
    if action != Action::Archive {
        let probe = ItemText {
            id: ItemId::generate(),
            kind,
            title: title.clone(),
            body: body.clone(),
            tags: tags.clone(),
            owner: None,
            review_by: review_by.clone(),
            supersedes: None,
        };
        if let Err(e) = apply(
            &format!("{}/probe.md", scope.dir()),
            None,
            &Edit::Append(probe),
        ) {
            return invalid(e.to_string());
        }
    }

    let conflicts = if action == Action::Archive {
        Vec::new()
    } else {
        find_conflicts(
            index.corpus(),
            &scope,
            kind,
            &title,
            &tags,
            args.item_id.as_deref(),
        )
    };

    let hash = content_hash(&ContentKey {
        action: action.as_str(),
        item_id: args.item_id.as_deref(),
        scope: scope.to_string(),
        kind: kind.as_str(),
        title: &title,
        body: &body,
        tags: &tags,
        review_by: review_by.as_deref(),
    });
    let new = NewProposal {
        action,
        item_id: args.item_id.clone(),
        base_revision: target.map(|t| t.revision),
        scope: scope.to_string(),
        kind: kind.to_string(),
        title,
        body,
        tags,
        review_by,
        reason,
        evidence,
        conflicts: serde_json::to_value(&conflicts).unwrap_or_default(),
        content_hash: hash,
        redacted: redacted as i32,
    };
    let (row, duplicate) = match store.create_proposal(who, &new, MAX_OPEN_PER_AGENT).await? {
        Proposed::New(r) => (r, false),
        Proposed::Duplicate(r) => (r, true),
        Proposed::TooMany { open } => {
            return invalid(format!(
                "你还有 {open} 条提议没处理，最多 {MAX_OPEN_PER_AGENT} 条；等人处理了再提"
            ));
        }
    };
    let conflicts: Vec<Conflict> =
        serde_json::from_value(row.conflicts.clone()).unwrap_or_default();
    let note = if duplicate {
        "一样的内容已经有一条提议在等审核，没有再建".to_owned()
    } else {
        let mut n = "已记下，等人审核；批准以前不会出现在上下文包里".to_owned();
        if row.redacted > 0 {
            n.push_str(&format!(
                "。内容里有 {} 处像密钥的东西，已经抹掉",
                row.redacted
            ));
        }
        n
    };
    Ok(Proposal {
        result_item_id: item_id_of(&row),
        proposal_id: row.id,
        status: row.status,
        duplicate,
        action: row.action,
        item_id: row.item_id,
        scope: row.scope,
        kind: row.kind,
        title: row.title,
        conflicts,
        redacted: row.redacted,
        note,
    })
}

/// 算重复用的内容：理由不算（同样的改动换个说法也算重复）。
#[derive(Serialize)]
struct ContentKey<'a> {
    action: &'static str,
    item_id: Option<&'a str>,
    scope: String,
    kind: &'static str,
    title: &'a str,
    body: &'a str,
    tags: &'a [String],
    review_by: Option<&'a str>,
}

fn content_hash(k: &ContentKey<'_>) -> String {
    let json = serde_json::to_vec(k).expect("序列化内存里的结构不会失败");
    Sha256::digest(&json)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
