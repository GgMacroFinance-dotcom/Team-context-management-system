//! 给人新写的、还没有 id 的条目补 id（`docs/architecture.md` 第 2 节）。
//!
//! - [`assign_all`]：`tcms assign-ids`，扫 HEAD 里所有的文件。
//! - [`assign_since`]：从远端拉下来新提交以后（[`crate::sync`]），只扫拉下来改到的文件。人推上去的新标题
//!   就这样拿到 id，补 id 的提交跟着推回去。
//!
//! 只插入 id，文件其余部分逐字节不变（[`tcms_core::assign_ids`]）。不在范围目录里的文件不管。

use serde::{Deserialize, Serialize};
use tcms_core::{assign_ids, scope_of_path};
use tcms_git::{Author, Change, ContextRepo, FileEdit, ObjectId};

/// 补上 id 的一条。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignedId {
    pub path: String,
    pub id: String,
    pub title: String,
}

/// 一次补 id 的提交。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdsAssigned {
    pub commit: String,
    pub items: Vec<AssignedId>,
}

/// 给 HEAD 里所有文件中没有 id 的条目补 id，做成一个提交。没有要补的（或者还没有提交）返回 `None`。
pub fn assign_all(
    repo: &ContextRepo,
    author: &Author,
) -> Result<Option<IdsAssigned>, tcms_git::Error> {
    let Some(head) = repo.head()? else {
        return Ok(None);
    };
    let files = repo.files(head)?;
    assign_in(repo, head, files, "", author)
}

/// 从 `before` 到 HEAD 新增、改过的文件里，给没有 id 的条目补 id，做成一个提交。`before` 为空表示从空仓库算起。
/// 没有要补的返回 `None`。
pub fn assign_since(
    repo: &ContextRepo,
    before: Option<ObjectId>,
    author: &Author,
) -> Result<Option<IdsAssigned>, tcms_git::Error> {
    let Some(head) = repo.head()? else {
        return Ok(None);
    };
    if before == Some(head) {
        return Ok(None);
    }
    let mut files = repo.files(head)?;
    let changed: std::collections::BTreeSet<String> = repo
        .changes(before, head)?
        .into_iter()
        .filter_map(|c| match c {
            Change::Upserted(p) => Some(p),
            Change::Deleted(_) => None,
        })
        .collect();
    files.retain(|path, _| changed.contains(path));
    assign_in(repo, head, files, "远端新来的 ", author)
}

fn assign_in(
    repo: &ContextRepo,
    head: ObjectId,
    files: impl IntoIterator<Item = (String, ObjectId)>,
    label: &str,
    author: &Author,
) -> Result<Option<IdsAssigned>, tcms_git::Error> {
    let mut edits = Vec::new();
    let mut items = Vec::new();
    for (path, blob) in files {
        if scope_of_path(&path).is_none() {
            continue;
        }
        let Ok(text) = String::from_utf8(repo.read_blob(blob)?) else {
            continue;
        };
        if let Some((new, ids)) = assign_ids(&path, &text) {
            items.extend(ids.into_iter().map(|(id, title)| AssignedId {
                path: path.clone(),
                id: id.to_string(),
                title,
            }));
            edits.push(FileEdit {
                path,
                content: Some(new.into_bytes()),
            });
        }
    }
    if edits.is_empty() {
        return Ok(None);
    }
    let lines: Vec<String> = items
        .iter()
        .map(|i| format!("{}  {}  {}", i.path, i.id, i.title))
        .collect();
    let message = format!("补 id：{label}{} 条\n\n{}\n", items.len(), lines.join("\n"));
    // 照着扫的那个 HEAD 提交：中间 HEAD 变了就不提交。
    let commit = repo.commit_on(Some(head), &edits, &message, author)?;
    Ok(Some(IdsAssigned {
        commit: commit.to_string(),
        items,
    }))
}
