//! 审核页用的 JSON 接口。出错时回 `{"error": "一句话", "code": "...", "current": 版本号?}`。

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Path, Query, State};
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tcms_core::{Kind, Scope, utc_today};
use tcms_git::Author;
use tcms_search::{SearchOptions, Visibility};
use tcms_store::{ItemRow, ProposalRow, Reviewer};
use tcms_write::{AcceptOptions, Conflict, ItemEdit, default_path, proposal_diff};

use crate::AppState;
use crate::writer::write_error;

/// 浏览时一次最多列几条。
const MAX_LIST: usize = 500;

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
    Ok(Json(json!({
        "proposal": p,
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
    Ok(Json(json!({"item": row, "superseded_by": superseded_by})))
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
