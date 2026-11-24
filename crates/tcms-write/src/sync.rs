//! 和远端同步（`docs/architecture.md` 第 3 节“远端”）。
//!
//! - 批准、改单条：排进提交队列以后，先从远端拉（快进或合并），再照着 HEAD 改、提交，记下以后推上去。
//!   合并不了（两边改了同一处）时停下来，提议还是待审核，等人合并；连不上远端时照样在本地提交，下次同步再推。
//! - [`sync`]：`tcms sync` 和服务里定时做的，排进同一个提交队列：拉、把索引带到 HEAD、推。
//!   人推到远端的改动就是这样进索引的。
//! - 拉下来新提交时，先给拉下来改到的文件里没有 id 的条目补 id（[`crate::assign_since`]），
//!   做成一个“补 id”的提交，和别的提交一起推回去。

use serde::{Deserialize, Serialize};
use tcms_git::{Author, ContextRepo, ObjectId, Pulled, Remote, RemoteError};
use tcms_store::Store;

use crate::accept::WriteError;
use crate::ids::{IdsAssigned, assign_since};

/// 和远端同步的结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteReport {
    /// 远端（地址里的用户名、密码换成了 `***`）。
    pub remote: String,
    /// 拉的结果；没拉成时为空，原因在 `errors`。
    pub pulled: Option<String>,
    /// 拉的时候 HEAD 变了（快进或合并），索引跟着更新了。
    pub head_moved: bool,
    /// 推的结果；没推成时为空，原因在 `errors`。
    pub pushed: Option<String>,
    /// 这次推上去了新的提交（不是“远端已经是最新的”）。
    pub pushed_new: bool,
    /// 两边都改了同一处、合并不了，要人合并。
    pub needs_merge: bool,
    /// 拉下来的条目里有没有 id 的，补上了，做了这个提交。
    #[serde(default)]
    pub ids: Option<IdsAssigned>,
    /// 拉、补 id、推出的错。提交已经在本地，下次同步时再推。
    pub errors: Vec<String>,
}

impl RemoteReport {
    fn new(remote: &Remote) -> Self {
        Self {
            remote: remote.display(),
            ..Self::default()
        }
    }

    /// 一句话：给命令行和日志用。
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(p) = &self.pulled {
            parts.push(format!("拉：{p}"));
        }
        if let Some(ids) = &self.ids {
            parts.push(format!(
                "补 id：远端新来的 {} 条，提交 {}",
                ids.items.len(),
                &ids.commit[..12.min(ids.commit.len())]
            ));
        }
        if let Some(p) = &self.pushed {
            parts.push(format!("推：{p}"));
        }
        parts.extend(self.errors.iter().cloned());
        format!("远端 {}：{}", self.remote, parts.join("；"))
    }

    /// 记下拉的结果；HEAD 变了就给拉下来的条目补 id。补 id 出错只记下来。
    fn pulled(
        &mut self,
        p: &Pulled,
        before: Option<ObjectId>,
        repo: &ContextRepo,
        author: &Author,
    ) {
        self.head_moved = p.moved_head();
        self.pulled = Some(p.to_string());
        if self.head_moved {
            match assign_since(repo, before, author) {
                Ok(ids) => self.ids = ids,
                Err(e) => self.errors.push(format!("补 id：{e}")),
            }
        }
    }
}

fn workdir(repo: &ContextRepo) -> Result<std::path::PathBuf, WriteError> {
    Ok(repo.workdir().ok_or(tcms_git::Error::Bare)?.to_owned())
}

/// 提交之前从远端拉。合并不了时返回错误（什么都没改）；别的错（连不上、超时）记下来，接着在本地做。
pub(crate) async fn pull_before(
    remote: Option<&Remote>,
    repo: &ContextRepo,
    author: &Author,
) -> Result<Option<RemoteReport>, WriteError> {
    let Some(remote) = remote else {
        return Ok(None);
    };
    let mut report = RemoteReport::new(remote);
    let before = repo.head()?;
    match remote.pull(&workdir(repo)?, author).await {
        Ok(p) => report.pulled(&p, before, repo, author),
        Err(e @ RemoteError::NeedsMerge { .. }) => return Err(WriteError::Remote(e)),
        Err(e) => report.errors.push(format!("拉取：{e}")),
    }
    Ok(Some(report))
}

/// 提交以后推上去。推不上去不算失败，记在结果里。
pub(crate) async fn push_after(
    remote: Option<&Remote>,
    repo: &ContextRepo,
    report: &mut Option<RemoteReport>,
) {
    let (Some(remote), Some(report)) = (remote, report.as_mut()) else {
        return;
    };
    let dir = match workdir(repo) {
        Ok(d) => d,
        Err(e) => {
            report.errors.push(format!("推送：{e}"));
            return;
        }
    };
    match remote.push(&dir).await {
        Ok(p) => {
            report.pushed_new = matches!(p, tcms_git::Pushed::Pushed { .. });
            report.pushed = Some(p.to_string());
        }
        Err(e) => report.errors.push(format!("推送：{e}")),
    }
}

/// 同步一次：排进提交队列，从远端拉（拉下来的条目没有 id 的补上），把索引带到 HEAD，再推。
/// 合并不了时不推，`needs_merge` 为真。
/// 索引出错时返回错误；拉、推出的错都在结果的 `errors` 里。
pub async fn sync(
    repo: &ContextRepo,
    store: &Store,
    remote: &Remote,
    author: &Author,
) -> Result<RemoteReport, WriteError> {
    let dir = workdir(repo)?;
    // 和批准、改单条排同一个队：拉的时候不会有人在提交。放掉（不 finish）就是什么都没记。
    let _queue = store.begin_commit().await?;
    let mut report = RemoteReport::new(remote);
    let before = repo.head()?;
    match remote.pull(&dir, author).await {
        Ok(p) => report.pulled(&p, before, repo, author),
        Err(e) => {
            report.needs_merge = matches!(e, RemoteError::NeedsMerge { .. });
            report.errors.push(format!("拉取：{e}"));
        }
    }
    tcms_index::reindex(repo, store, false).await?;
    if !report.needs_merge {
        let mut wrapped = Some(report);
        push_after(Some(remote), repo, &mut wrapped).await;
        report = wrapped.expect("上面放进去的");
    }
    Ok(report)
}
