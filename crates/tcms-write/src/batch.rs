//! 在提交队列里连着做几个提交（和 AFFiNE 同步用，`docs/affine-sync.md` 5.2）。
//!
//! 步骤和批准、改单条一样：拿提交队列的锁 → 配了远端时先拉 → 把索引带到 HEAD → 照着 HEAD 改、提交（可以几次，
//! 每次都要求 HEAD 没被别人动过）→ 有提交时索引、记审计 → 配了远端时推上去。中间要等别的事（比如连 AFFiNE），
//! 都在锁里，批准、改单条排在后面。

use tcms_git::{Author, ContextRepo, FileEdit, ObjectId, Remote};
use tcms_store::{CommitLock, Store};

use crate::accept::{WriteError, read_at};
use crate::sync::{RemoteReport, pull_before, push_after};

/// 拿着提交队列的锁。放掉（不 [`Batch::finish`]）就是什么都没记，已经做的提交还在。
pub struct Batch {
    lock: CommitLock,
    report: Option<RemoteReport>,
    remote: Option<Remote>,
    head: Option<ObjectId>,
    commits: Vec<String>,
}

/// 开始：拿锁、拉、把索引带到 HEAD。合并不了时返回错误。
pub async fn begin_batch(
    repo: &ContextRepo,
    store: &Store,
    remote: Option<&Remote>,
    author: &Author,
) -> Result<Batch, WriteError> {
    let lock = store.begin_commit().await?;
    let report = pull_before(remote, repo, author).await?;
    tcms_index::reindex(repo, store, false).await?;
    let head = repo.head()?;
    Ok(Batch {
        lock,
        report,
        remote: remote.cloned(),
        head,
        commits: Vec::new(),
    })
}

impl Batch {
    /// 现在照着的提交。
    pub fn head(&self) -> Option<ObjectId> {
        self.head
    }

    /// 读现在这个提交里的一个文件，没有时是 `None`。
    pub fn read(&self, repo: &ContextRepo, path: &str) -> Result<Option<String>, WriteError> {
        read_at(repo, self.head, path)
    }

    /// 做一个提交，接着照着它改。
    pub fn commit(
        &mut self,
        repo: &ContextRepo,
        edits: &[FileEdit],
        message: &str,
        author: &Author,
    ) -> Result<ObjectId, WriteError> {
        let id = repo.commit_on(self.head, edits, message, author)?;
        self.head = Some(id);
        self.commits.push(id.to_string());
        Ok(id)
    }

    /// 这次做了的提交。
    pub fn commits(&self) -> &[String] {
        &self.commits
    }

    /// 做完：有提交时索引、记进审计、推上去；没有提交时只放掉锁。返回和远端打交道的结果（没配远端时为空）。
    pub async fn finish(
        self,
        repo: &ContextRepo,
        store: &Store,
        actor: &str,
        action: &str,
        target: &str,
        detail: serde_json::Value,
    ) -> Result<Option<RemoteReport>, WriteError> {
        let Batch {
            lock,
            mut report,
            remote,
            commits,
            ..
        } = self;
        if commits.is_empty() {
            drop(lock);
            return Ok(report);
        }
        tcms_index::reindex(repo, store, false).await?;
        lock.finish(actor, action, target, detail).await?;
        push_after(remote.as_ref(), repo, &mut report).await;
        Ok(report)
    }
}
