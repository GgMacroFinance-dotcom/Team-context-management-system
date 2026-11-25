//! 审核页里的“和 AFFiNE 同步”（`docs/affine-sync.md` 7.5）：跟着同步现在怎么样了、等人处理的条目
//! （git 的、AFFiNE 的、上次同步完的底稿并排）、停着的文档、在 AFFiNE 里只能看的条目。
//!
//! - 处理等人处理的和 `tcms affine resolve` 一样改底稿（[`tcms_affine::resolve`]），记审计 `affine.resolve`；
//!   两边都改了的还可以改出合并后的：先在 git 里改成那样（和改单条一样是一个提交），再按“留 git 的”处理。
//!   这个服务在跟着同步时马上同步那一篇；没跟着同步时，下次 `tcms affine sync` 写过去。
//! - 停着的文档接着做（记审计 `affine.resume`）要这个服务在跟着同步，和 `tcms affine sync --resume <路径>` 一样。

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Path, State};
use http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tcms_affine::plan::{Reason, readonly};
use tcms_affine::{FollowLink, Keep};
use tcms_store::{AffineConflict, Reviewer};
use tcms_write::{ItemEdit, diff};

use crate::AppState;
use crate::api::{ApiError, ApiResult, author_for, body, not_found, store_error};

/// “最近的”列几条审计（同步、接上、处理、接着做）。
const RECENT: i64 = 20;

fn affine_error(e: tcms_affine::Error) -> ApiError {
    match e {
        tcms_affine::Error::Invalid(msg) => ApiError::new(StatusCode::CONFLICT, "conflict", msg),
        tcms_affine::Error::Store(e) => store_error(e),
        e => {
            eprintln!("审核页处理 AFFiNE 同步时出错：{e}");
            ApiError::internal(e.to_string())
        }
    }
}

/// 一边的样子：标题一行、空一行、正文。这一边删掉了是 `None`。
fn side(title: &Option<String>, body: &Option<String>) -> Option<Value> {
    let title = title.as_ref()?;
    let body = body.as_deref().unwrap_or("");
    Some(json!({"title": title, "body": body}))
}

fn side_text(title: &Option<String>, body: &Option<String>) -> Option<String> {
    let title = title.as_ref()?;
    let body = body.as_deref().unwrap_or("").trim_end();
    Some(format!("标题：{title}\n\n{body}\n"))
}

/// 一条等人处理的能怎么处理：`keep` 是交给接口的，`label` 写在按钮上，`then` 是下次同步时会怎样。
fn choices(reason: Reason) -> Vec<Value> {
    let c =
        |keep: &str, label: &str, then: &str| json!({"keep": keep, "label": label, "then": then});
    match reason {
        Reason::Both => vec![
            c("git", "留 git 的", "AFFiNE 里这一条改成 git 的样子"),
            c(
                "affine",
                "留 AFFiNE 的",
                "git 里这一条改成 AFFiNE 的样子（一个提交）",
            ),
        ],
        Reason::AffineDeleted => vec![
            c("git", "留下", "git 里改过的这一条重新写进 AFFiNE"),
            c("affine", "删掉", "git 里也归档这一条（能从历史里找回）"),
        ],
        Reason::GitDeleted => vec![
            c("git", "删掉", "AFFiNE 里也删掉这一条"),
            c(
                "affine",
                "留下",
                "AFFiNE 里的这一条当成新加的写进 git（新的 id）",
            ),
        ],
        Reason::Readonly => vec![c(
            "git",
            "改回 git 的",
            "AFFiNE 里这一条改回 git 的样子；这一条的写法（表格、图片……）AFFiNE 表示不全，要改就直接改上下文仓库里的 Markdown",
        )],
        Reason::Invalid => vec![
            c("affine", "再试一次", "下次同步时再写一次"),
            c("git", "用 git 的", "AFFiNE 里这一条改成 git 的样子"),
        ],
    }
}

fn conflict(c: &AffineConflict) -> Value {
    let reason = Reason::parse(&c.reason).unwrap_or(Reason::Invalid);
    let base = side_text(&c.base_title, &c.base_body).unwrap_or_default();
    let against_base =
        |t: &Option<String>, b: &Option<String>| side_text(t, b).map(|s| diff(&base, &s));
    json!({
        "id": c.id,
        "path": c.path,
        "item_id": c.item_id,
        "heading_id": c.heading_id,
        "reason": c.reason,
        "label": reason.label(),
        "message": c.message,
        "found_at": c.found_at,
        "title": c.git_title.as_ref().or(c.affine_title.as_ref()).or(c.base_title.as_ref()),
        "git": side(&c.git_title, &c.git_body),
        "affine": side(&c.affine_title, &c.affine_body),
        "base": side(&c.base_title, &c.base_body),
        "git_diff": against_base(&c.git_title, &c.git_body),
        "affine_diff": against_base(&c.affine_title, &c.affine_body),
        "choices": choices(reason),
        // 两边都在、都改了的，可以改出合并后的。
        "can_merge": reason == Reason::Both && c.item_id.is_some(),
    })
}

/// 这个服务没跟着同步时是 `null`。`now_ms` 是服务器现在的时间，网页拿它算“几分钟前”。
pub(crate) async fn overview(State(state): State<AppState>) -> ApiResult {
    let store = state.live.store();
    let st = tcms_affine::status(store).await.map_err(affine_error)?;
    let bases = store.affine_bases().await.map_err(store_error)?;
    let queued = state
        .affine
        .as_ref()
        .map(FollowLink::queued)
        .unwrap_or_default();
    let docs: Vec<Value> = st
        .docs
        .iter()
        .map(|d| {
            json!({
                "path": d.path,
                "doc_id": d.doc_id,
                "items": d.items,
                "paused": d.paused,
                "synced_at": d.synced_at,
                "resuming": queued.resume.contains(&d.path),
            })
        })
        .collect();
    let readonly: Vec<Value> = bases
        .iter()
        .filter(|b| readonly(&b.body))
        .map(|b| json!({"item_id": b.item_id, "path": b.path, "title": b.title}))
        .collect();
    Ok(Json(json!({
        "following": state.affine.is_some(),
        "follow": state.affine.as_ref().map(FollowLink::state),
        "now_ms": crate::writer::now_ms(),
        "scopes": st.scopes,
        "docs": docs,
        "conflicts": st.conflicts.iter().map(conflict).collect::<Vec<_>>(),
        "readonly": readonly,
        "recent": store.affine_audit(RECENT).await.map_err(store_error)?,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolveBody {
    /// `git`、`affine`，或者 `merged`（带上合并后的 `title`、`body`）。
    keep: String,
    title: Option<String>,
    body: Option<String>,
}

/// 处理一条等人处理的。回 `{path, following, edit?}`：`following` 是这个服务在跟着同步（马上同步那一篇），
/// `edit` 是改出合并后的时 git 里的那个提交（和改单条回的一样）。
pub(crate) async fn resolve(
    State(state): State<AppState>,
    Extension(who): Extension<Reviewer>,
    Path(id): Path<i64>,
    payload: Result<Json<ResolveBody>, JsonRejection>,
) -> ApiResult {
    let b = body(payload)?;
    let store = state.live.store();
    let actor = who.actor();
    let (keep, edit) = match b.keep.as_str() {
        "git" => (Keep::Git, None),
        "affine" => (Keep::Affine, None),
        "merged" => (Keep::Git, merge(&state, &who, id, b.title, b.body).await?),
        other => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid",
                format!("keep 只能是 git、affine、merged：{other}"),
            ));
        }
    };
    let path = tcms_affine::resolve(store, id, keep, &actor)
        .await
        .map_err(affine_error)?;
    if let Some(link) = &state.affine {
        let docs = store.affine_docs().await.map_err(store_error)?;
        if let Some(d) = docs.iter().find(|d| d.path == path) {
            link.sync_now(&d.doc_id);
        }
    }
    Ok(Json(json!({
        "path": path,
        "following": state.affine.is_some(),
        "edit": edit,
    })))
}

/// 改出合并后的：先在 git 里把这一条改成那样。处理之前这一条两边都不动（同步跳过等人处理的），
/// 所以改完再按“留 git 的”处理，下次同步把合并后的写进 AFFiNE。和 git 里一样的话不用改。
async fn merge(
    state: &AppState,
    who: &Reviewer,
    id: i64,
    title: Option<String>,
    text: Option<String>,
) -> Result<Option<Value>, ApiError> {
    let store = state.live.store();
    let Some(c) = store
        .affine_conflicts()
        .await
        .map_err(store_error)?
        .into_iter()
        .find(|c| c.id == id)
    else {
        return Err(not_found("条等人处理的", &id.to_string()));
    };
    let invalid = |m: &str| ApiError::new(StatusCode::BAD_REQUEST, "invalid", m);
    let (Some(item_id), "both") = (c.item_id.as_ref(), c.reason.as_str()) else {
        return Err(invalid("只有两边都改了的能改出合并后的"));
    };
    let (Some(title), Some(text)) = (title, text) else {
        return Err(invalid("合并后的要写标题和正文"));
    };
    let stale = || {
        ApiError::new(
            StatusCode::CONFLICT,
            "stale",
            "git 里这一条在记下以后又改过了，刷新一下看现在的",
        )
    };
    let Some(cur) = store.get_item(item_id).await.map_err(store_error)? else {
        return Err(stale());
    };
    let same_as_noted = cur.status == "active"
        && Some(&cur.title) == c.git_title.as_ref()
        && Some(cur.body.trim_end()) == c.git_body.as_deref().map(str::trim_end);
    if !same_as_noted {
        return Err(stale());
    }
    let change = ItemEdit {
        title: (title != cur.title).then_some(title),
        body: (text.trim_end() != cur.body.trim_end()).then_some(text),
        tags: None,
        review_by: None,
    };
    if change.title.is_none() && change.body.is_none() {
        return Ok(None);
    }
    let author = author_for(state, who);
    let done = state
        .writer
        .edit(item_id.clone(), cur.revision, change, who.actor(), author)
        .await?;
    Ok(Some(done))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResumeBody {
    path: String,
}

/// 停着的文档接着做（人看过了）。这个服务没跟着同步时回 409，用 `tcms affine sync --resume <路径>`。
pub(crate) async fn resume(
    State(state): State<AppState>,
    Extension(who): Extension<Reviewer>,
    payload: Result<Json<ResumeBody>, JsonRejection>,
) -> ApiResult {
    let b = body(payload)?;
    let Some(link) = &state.affine else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "not_following",
            format!(
                "这个服务没在跟着同步（没设 TCMS_AFFINE_WORKSPACE），在命令行用 tcms affine sync --resume {}",
                b.path
            ),
        ));
    };
    let store = state.live.store();
    let Some(d) = store
        .affine_docs()
        .await
        .map_err(store_error)?
        .into_iter()
        .find(|d| d.path == b.path)
    else {
        return Err(not_found("篇接上的文档", &b.path));
    };
    let Some(paused) = d.paused else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "not_paused",
            format!("{} 没有停着（可能已经接着做完了），刷新一下", d.path),
        ));
    };
    store
        .record_affine_audit(
            &who.actor(),
            "affine.resume",
            &d.path,
            json!({"doc_id": d.doc_id, "paused": paused}),
        )
        .await
        .map_err(store_error)?;
    link.resume(&d.path);
    Ok(Json(json!({"path": d.path})))
}
