//! 审核页用的 JSON 接口。出错时回 `{"error": "一句话", "code": "...", "current": 版本号?}`。

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Path, Query, State};
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tcms_core::{Kind, Scope, Teams, utc_today};
use tcms_git::{Author, ContextRepo};
use tcms_search::{SearchOptions, Visibility};
use tcms_store::{Identity, ItemRow, ProposalRow, Reviewer};
use tcms_write::{
    AcceptOptions, Conflict, ItemEdit, ProposeError, default_path, promotion_candidates,
    promotion_targets, proposal_diff, read_teams,
};

use crate::AppState;
use crate::writer::write_error;

/// 浏览时一次最多列几条。
const MAX_LIST: usize = 500;
/// 晋升候选一次最多列几组。
const MAX_CANDIDATES: usize = 50;

#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    current: Option<i32>,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            current: None,
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    pub(crate) fn with_current(mut self, current: i32) -> Self {
        self.current = Some(current);
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({"error": self.message, "code": self.code});
        if let Some(c) = self.current {
            body["current"] = json!(c);
        }
        (self.status, Json(body)).into_response()
    }
}

pub(crate) fn error(status: StatusCode, code: &'static str, message: &str) -> Response {
    ApiError::new(status, code, message).into_response()
}

type ApiResult = Result<Json<Value>, ApiError>;

fn store_error(e: tcms_store::Error) -> ApiError {
    eprintln!("审核页查数据库出错：{e}");
    ApiError::internal(format!("查数据库出错：{e}"))
}

fn not_found(what: &str, id: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("没有这{what}：{id}"),
    )
}

fn body<T>(payload: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    payload
        .map(|Json(b)| b)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid", format!("请求不对：{e}")))
}

/// 空字符串当作没写。
fn given(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.trim().is_empty())
}

fn author_for(state: &AppState, who: &Reviewer) -> Author {
    Author {
        name: who.name.clone(),
        email: state.author_email.clone(),
    }
}

pub(crate) async fn me(Extension(who): Extension<Reviewer>) -> Json<Value> {
    Json(json!({"name": who.name, "token_id": who.token_id}))
}

/// 和远端同步的情况：配的哪个远端、最近一次的结果、合并不了时怎么合并。`now_ms` 是服务器现在的时间，
/// 网页拿它算“几分钟前”，不受浏览器时钟的影响。
pub(crate) async fn remote(State(state): State<AppState>) -> ApiResult {
    let mut v = serde_json::to_value(state.writer.remote_status())
        .map_err(|e| ApiError::internal(e.to_string()))?;
    v["now_ms"] = json!(crate::writer::now_ms());
    Ok(Json(v))
}

#[derive(Deserialize)]
pub(crate) struct ListQuery {
    status: Option<String>,
}

fn summary(p: &ProposalRow) -> Value {
    json!({
        "id": p.id,
        "status": p.status,
        "action": p.action,
        "item_id": p.item_id,
        "scope": p.scope,
        "kind": p.kind,
        "title": p.title,
        "proposer_name": p.proposer_name,
        "proposer_token": p.proposer_token,
        "proposer_role": p.proposer_role,
        "created_at": p.created_at,
        "decided_at": p.decided_at,
        "decided_by": p.decided_by,
        "conflicts": p.conflicts.as_array().map_or(0, Vec::len),
        "redacted": p.redacted,
    })
}

pub(crate) async fn proposals(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult {
    let filter = match q.status.as_deref().unwrap_or("open") {
        "all" => None,
        s @ ("open" | "accepted" | "rejected") => Some(s),
        other => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid",
                format!("status 只能是 open、accepted、rejected、all：{other}"),
            ));
        }
    };
    let rows = state
        .live
        .store()
        .list_proposals(filter)
        .await
        .map_err(store_error)?;
    Ok(Json(
        json!({"proposals": rows.iter().map(summary).collect::<Vec<_>>()}),
    ))
}

fn item_brief(r: &ItemRow) -> Value {
    json!({
        "id": r.id,
        "revision": r.revision,
        "status": r.status,
        "scope": r.scope,
        "kind": r.kind,
        "title": r.title,
        "body": r.body,
        "tags": r.tags,
        "review_by": r.review_by,
        "path": r.path,
    })
}

pub(crate) async fn proposal(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let store = state.live.store();
    let Some(p) = store.get_proposal(&id).await.map_err(store_error)? else {
        return Err(not_found("条提议", &id));
    };
    let current: Option<ItemRow> = match &p.item_id {
        Some(item) => store.get_item(item).await.map_err(store_error)?,
        None => None,
    };
    let stale = match (&current, p.base_revision) {
        (Some(c), Some(base)) => c.revision != base,
        _ => false,
    };
    let diff = proposal_diff(&p, current.as_ref());
    // 相近的条目：带上它现在的样子，好对照着看。
    let found: Vec<Conflict> = serde_json::from_value(p.conflicts.clone()).unwrap_or_default();
    let mut conflicts = Vec::with_capacity(found.len());
    for c in &found {
        let now = store.get_item(&c.id).await.map_err(store_error)?;
        conflicts.push(json!({
            "id": c.id,
            "revision": c.revision,
            "scope": c.scope,
            "kind": c.kind,
            "title": c.title,
            "similarity": c.similarity,
            "current": now.as_ref().map(item_brief),
        }));
    }
    let default_file = match (
        p.action.as_str(),
        p.scope.parse::<Scope>(),
        p.kind.parse::<Kind>(),
    ) {
        ("supersede", _, _) if current.as_ref().is_some_and(|c| c.scope == p.scope) => {
            current.as_ref().map(|c| c.path.clone())
        }
        ("create" | "supersede", Ok(scope), Ok(kind)) => Some(default_path(&scope, kind)),
        _ => None,
    };
    // 换了范围的取代就是晋升：写明从哪里到哪里。
    let promotion = match (p.action.as_str(), &current) {
        ("supersede", Some(c)) if c.scope != p.scope => {
            Some(json!({"from": c.scope, "to": p.scope}))
        }
        _ => None,
    };
    Ok(Json(json!({
        "proposal": p,
        "promotion": promotion,
        "current": current.as_ref().map(item_brief),
        "stale": stale,
        "diff": diff,
        "conflicts": conflicts,
        "default_file": default_file,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptBody {
    title: Option<String>,
    body: Option<String>,
    file: Option<String>,
    /// 看过的是现在的第几版（条目在提议之后被人改过时）。
    over: Option<i32>,
    note: Option<String>,
}

pub(crate) async fn accept(
    State(state): State<AppState>,
    Extension(who): Extension<Reviewer>,
    Path(id): Path<String>,
    payload: Result<Json<AcceptBody>, JsonRejection>,
) -> ApiResult {
    let b = body(payload)?;
    let opts = AcceptOptions {
        actor: who.actor(),
        author: author_for(&state, &who),
        title: b.title,
        body: b.body,
        file: given(b.file),
        over_revision: b.over,
        note: given(b.note),
        // 远端由写入线程填。
        remote: None,
    };
    state.writer.accept(id, opts).await.map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RejectBody {
    note: Option<String>,
}

pub(crate) async fn reject(
    State(state): State<AppState>,
    Extension(who): Extension<Reviewer>,
    Path(id): Path<String>,
    payload: Result<Json<RejectBody>, JsonRejection>,
) -> ApiResult {
    let b = body(payload)?;
    let store = state.live.store();
    tcms_write::reject(store, &id, &who.actor(), given(b.note).as_deref())
        .await
        .map_err(write_error)?;
    let p = store.get_proposal(&id).await.map_err(store_error)?;
    Ok(Json(json!({"proposal": p})))
}

#[derive(Deserialize)]
pub(crate) struct DueQuery {
    /// 往后多少天以内到期的也列出来，默认 0（今天以前到期的）。
    within: Option<i32>,
}

pub(crate) async fn due(State(state): State<AppState>, Query(q): Query<DueQuery>) -> ApiResult {
    let today = utc_today();
    let within = q.within.unwrap_or(0).clamp(0, 366);
    let rows = state
        .live
        .store()
        .items_due(&today, within)
        .await
        .map_err(store_error)?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id, "revision": r.revision, "scope": r.scope, "kind": r.kind,
                "title": r.title, "review_by": r.review_by, "owner": r.owner, "path": r.path,
            })
        })
        .collect();
    Ok(Json(
        json!({"today": today, "within": within, "items": items}),
    ))
}

#[derive(Deserialize)]
pub(crate) struct ItemsQuery {
    scope: Option<String>,
    kind: Option<String>,
    q: Option<String>,
}

pub(crate) async fn items(State(state): State<AppState>, Query(q): Query<ItemsQuery>) -> ApiResult {
    let bad = |m: String| ApiError::new(StatusCode::BAD_REQUEST, "invalid", m);
    let scope = match given(q.scope) {
        Some(s) => Some(s.parse::<Scope>().map_err(bad)?),
        None => None,
    };
    let kind = match given(q.kind) {
        Some(k) => Some(k.parse::<Kind>().map_err(bad)?),
        None => None,
    };
    let index = state.live.current();
    let corpus = index.corpus();
    let mut scopes = std::collections::BTreeMap::<String, usize>::new();
    let mut kinds = std::collections::BTreeMap::<&'static str, usize>::new();
    for it in corpus.items() {
        *scopes.entry(it.scope.to_string()).or_default() += 1;
        *kinds.entry(it.kind.as_str()).or_default() += 1;
    }
    let keep = |it: &&tcms_search::CorpusItem| {
        scope.as_ref().is_none_or(|s| &it.scope == s) && kind.is_none_or(|k| it.kind == k)
    };
    let picked: Vec<&tcms_search::CorpusItem> = match given(q.q) {
        Some(text) => {
            let hits = index
                .search(
                    &text,
                    &Visibility::All,
                    &SearchOptions {
                        kind,
                        limit: tcms_search::MAX_LIMIT,
                    },
                )
                .map_err(|e| bad(e.to_string()))?;
            hits.iter()
                .filter_map(|h| corpus.get(&h.id))
                .filter(keep)
                .collect()
        }
        None => {
            let mut v: Vec<_> = corpus.items().iter().filter(keep).collect();
            v.sort_by(|a, b| {
                (
                    a.scope.to_string(),
                    a.kind.as_str(),
                    &a.path,
                    &a.title,
                    &a.id,
                )
                    .cmp(&(
                        b.scope.to_string(),
                        b.kind.as_str(),
                        &b.path,
                        &b.title,
                        &b.id,
                    ))
            });
            v
        }
    };
    let total = picked.len();
    let items: Vec<Value> = picked
        .iter()
        .take(MAX_LIST)
        .map(|it| {
            json!({
                "id": it.id, "revision": it.revision, "scope": it.scope.to_string(),
                "kind": it.kind.as_str(), "title": it.title, "tags": it.tags,
                "review_by": it.review_by, "path": it.path,
            })
        })
        .collect();
    Ok(Json(json!({
        "items": items,
        "total": total,
        "limit": MAX_LIST,
        "scopes": scopes.iter().map(|(s, n)| json!({"scope": s, "count": n})).collect::<Vec<_>>(),
        "kinds": kinds.iter().map(|(k, n)| json!({"kind": k, "count": n})).collect::<Vec<_>>(),
        "commit": corpus.commit,
    })))
}

pub(crate) async fn item(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let Some(row) = state
        .live
        .store()
        .get_item(&id)
        .await
        .map_err(store_error)?
    else {
        return Err(not_found("一条", &id));
    };
    let index = state.live.current();
    let superseded_by: Vec<&str> = index
        .corpus()
        .items()
        .iter()
        .filter(|it| it.supersedes.as_deref() == Some(id.as_str()))
        .map(|it| it.id.as_str())
        .collect();
    // 生效的条目能提到哪些更大的范围（晋升）。
    let promote_to: Vec<String> = match (row.status.as_str(), row.scope.parse::<Scope>()) {
        ("active", Ok(scope)) => {
            let teams = teams(&state, index.corpus().commit.as_deref());
            promotion_targets(&scope, teams.as_ref())
                .iter()
                .map(ToString::to_string)
                .collect()
        }
        _ => Vec::new(),
    };
    Ok(Json(json!({
        "item": row,
        "superseded_by": superseded_by,
        "promote_to": promote_to,
    })))
}

/// 索引到的那个提交里的 `teams.yaml`。读不了时当没有（岗位只能提到全公司），写一行日志。
fn teams(state: &AppState, commit: Option<&str>) -> Option<Teams> {
    let commit = commit?;
    let read = ContextRepo::open(state.writer.repo())
        .map_err(|e| e.to_string())
        .and_then(|repo| read_teams(&repo, commit));
    match read {
        Ok(t) => t,
        Err(e) => {
            eprintln!("读 teams.yaml 出错（岗位的条目这次只能提到全公司）：{e}");
            None
        }
    }
}

/// 系统找的晋升候选：几个岗位（或团队、项目）都写了的相近条目，建议提到哪里（`docs/spec.md` 第 5 节）。
/// 哪条已经有晋升提议在等审核，带上那条提议的 id（`pending`）。
pub(crate) async fn promotions(State(state): State<AppState>) -> ApiResult {
    let index = state.live.current();
    let corpus = index.corpus();
    let teams = teams(&state, corpus.commit.as_deref());
    let all = promotion_candidates(corpus, teams.as_ref());
    let open = state
        .live
        .store()
        .list_proposals(Some("open"))
        .await
        .map_err(store_error)?;
    // 等审核的晋升：换了范围的取代。
    let pending = |id: &str, scope: &str| -> Option<String> {
        open.iter()
            .find(|p| {
                p.action == "supersede" && p.item_id.as_deref() == Some(id) && p.scope != scope
            })
            .map(|p| p.id.clone())
    };
    let candidates: Vec<Value> = all
        .iter()
        .take(MAX_CANDIDATES)
        .map(|c| {
            let items: Vec<Value> = c
                .items
                .iter()
                .map(|it| {
                    json!({
                        "id": it.id, "scope": it.scope, "title": it.title, "revision": it.revision,
                        "pending": pending(&it.id, &it.scope),
                    })
                })
                .collect();
            json!({"to": c.to, "kind": c.kind, "items": items})
        })
        .collect();
    Ok(Json(json!({
        "total": all.len(),
        "limit": MAX_CANDIDATES,
        "commit": corpus.commit,
        "candidates": candidates,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromoteBody {
    /// 提到哪个范围：`team:<团队>` 或 `org`。
    to: String,
    reason: String,
}

/// 晋升：做成一条换了范围的取代提议，提议人是审核的人，进待审核的列表，要再批准一次。
pub(crate) async fn promote(
    State(state): State<AppState>,
    Extension(who): Extension<Reviewer>,
    Path(id): Path<String>,
    payload: Result<Json<PromoteBody>, JsonRejection>,
) -> ApiResult {
    let b = body(payload)?;
    let to: Scope =
        b.to.parse()
            .map_err(|e: String| ApiError::new(StatusCode::BAD_REQUEST, "invalid", e))?;
    let index = state.live.current();
    let teams = teams(&state, index.corpus().commit.as_deref());
    let by = Identity {
        token_id: who.token_id.clone(),
        name: who.name.clone(),
        // 岗位空着：是人提的，不是哪个岗位的 agent。
        role: String::new(),
        team: None,
        projects: Vec::new(),
    };
    match tcms_write::promote(
        state.live.store(),
        &index,
        &id,
        &to,
        &b.reason,
        teams.as_ref(),
        &by,
    )
    .await
    {
        Ok(p) => Ok(Json(json!(p))),
        Err(ProposeError::Invalid(m)) => Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid", m)),
        Err(ProposeError::Store(e)) => Err(store_error(e)),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditBody {
    /// 改的人看到的版本号。
    base_revision: i32,
    title: Option<String>,
    body: Option<String>,
    tags: Option<Vec<String>>,
    review_by: Option<String>,
}

pub(crate) async fn edit(
    State(state): State<AppState>,
    Extension(who): Extension<Reviewer>,
    Path(id): Path<String>,
    payload: Result<Json<EditBody>, JsonRejection>,
) -> ApiResult {
    let b = body(payload)?;
    let change = ItemEdit {
        title: b.title,
        body: b.body,
        tags: b.tags,
        review_by: b.review_by,
    };
    let author = author_for(&state, &who);
    state
        .writer
        .edit(id, b.base_revision, change, who.actor(), author)
        .await
        .map(Json)
}
