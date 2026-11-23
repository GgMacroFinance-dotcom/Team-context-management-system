//! 人在审核页里改单条的纯文本（`docs/spec.md` 6.4）：标题、正文、标签、复核日期。
//!
//! 和批准提议排在同一个提交队列里，步骤也一样：把索引带到 HEAD → 看版本号有没有变 → 改文件 → 提交（HEAD 没变才提交）→ 索引。
//! 这是人写的修改，可以改规则；不过脱敏（人自己负责）。类型、范围不在这里改。

use tcms_core::edit::{Edit, ItemText, apply};
use tcms_core::{ItemId, normalize_body, normalize_title, parse_file, valid_date};
use tcms_git::{Author, ContextRepo, FileEdit};
use tcms_store::Store;

use crate::accept::{WriteError, read_at};

/// 改成什么样。`None` 的字段不动。
#[derive(Debug, Clone, Default)]
pub struct ItemEdit {
    pub title: Option<String>,
    pub body: Option<String>,
    pub tags: Option<Vec<String>>,
    /// `Some("")` 表示去掉复核日期。
    pub review_by: Option<String>,
}

/// 改完的结果。
#[derive(Debug, Clone)]
pub struct Edited {
    pub item: String,
    pub revision: i32,
    pub commit: String,
    pub path: String,
}

/// 改一条。`base_revision` 是改的人看到的版本号，落后了（别人刚改过）就停下来。
pub async fn edit_item(
    repo: &ContextRepo,
    store: &Store,
    id: &str,
    base_revision: i32,
    change: &ItemEdit,
    actor: &str,
    author: &Author,
) -> Result<Edited, WriteError> {
    let lock = store.begin_commit().await?;
    tcms_index::reindex(repo, store, false).await?;
    let head = repo.head()?;
    let row = match store.get_item(id).await? {
        Some(r) if r.status == "active" => r,
        _ => return Err(WriteError::TargetGone(id.to_owned())),
    };
    if row.revision != base_revision {
        return Err(WriteError::Stale {
            item: id.to_owned(),
            base: base_revision,
            current: row.revision,
        });
    }
    let current = read_at(repo, head, &row.path)?.unwrap_or_default();
    let parsed = parse_file(&row.path, &current);
    let Some(old) = parsed
        .items
        .iter()
        .find(|it| it.id.as_ref().map(ItemId::as_str) == Some(id))
    else {
        return Err(WriteError::TargetGone(id.to_owned()));
    };
    let Some(mut text) = ItemText::from_parsed(old) else {
        return Err(WriteError::TargetGone(id.to_owned()));
    };
    if let Some(t) = &change.title {
        text.title = normalize_title(t);
    }
    if let Some(b) = &change.body {
        text.body = normalize_body(b);
    }
    if let Some(tags) = &change.tags {
        let mut seen = std::collections::HashSet::new();
        text.tags = tags
            .iter()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty() && seen.insert(t.clone()))
            .collect();
        if text.tags.iter().any(|t| t.contains([',', '\n', '"'])) {
            return Err(WriteError::Invalid(
                "标签里不能有逗号、换行、引号".to_owned(),
            ));
        }
    }
    if let Some(d) = &change.review_by {
        let d = d.trim();
        text.review_by = match d {
            "" => None,
            d if valid_date(d) => Some(d.to_owned()),
            d => return Err(WriteError::Invalid(format!("复核日期不是 YYYY-MM-DD：{d}"))),
        };
    }
    if text.title.is_empty() {
        return Err(WriteError::Invalid("标题不能是空的".to_owned()));
    }
    if text.matches(old) {
        return Err(WriteError::Invalid("没有改动".to_owned()));
    }
    let title = text.title.clone();
    let content = apply(&row.path, Some(&current), &Edit::Replace(text))?;
    let message = format!(
        "{}\n\nItem: {id}\nEdited-by: {actor}\n",
        format!("人改：{title}")
            .chars()
            .take(72)
            .collect::<String>()
    );
    let commit = repo.commit_on(
        head,
        &[FileEdit {
            path: row.path.clone(),
            content: content.map(String::into_bytes),
        }],
        &message,
        author,
    )?;
    tcms_index::reindex(repo, store, false).await?;
    let revision = store
        .get_item(id)
        .await?
        .map(|r| r.revision)
        .unwrap_or(row.revision);
    let commit = commit.to_string();
    let detail = serde_json::json!({"commit": commit, "revision": revision, "path": row.path});
    lock.finish(actor, "item.edit", id, detail).await?;
    Ok(Edited {
        item: id.to_owned(),
        revision,
        commit,
        path: row.path,
    })
}
