//! 写 git 的那个线程：批准提议、改单条、和远端同步。
//!
//! gix 的仓库句柄不能在线程之间共享，批准的过程又要跨好几次等数据库，所以放在一个专门的线程里，
//! 自己的单线程 tokio 运行时、自己的连接池（和服务的提交队列锁是同一把），一次做一件。
//! 每件事都重新打开上下文仓库，读到的总是磁盘上最新的样子。配了远端时，批准、改单条先拉后推（`tcms_write`）。

use std::path::PathBuf;

use serde_json::{Value, json};
use tcms_git::{Author, ContextRepo, Remote, RemoteError};
use tcms_store::Store;
use tcms_write::{AcceptOptions, Committer, ItemEdit, RemoteReport, WriteError};
use tokio::sync::{mpsc, oneshot};

use crate::Error;
use crate::api::ApiError;

type Reply = oneshot::Sender<Result<Value, ApiError>>;

enum Job {
    Accept {
        id: String,
        opts: AcceptOptions,
    },
    Edit {
        id: String,
        base_revision: i32,
        change: ItemEdit,
        actor: String,
        author: Author,
    },
    Sync {
        author: Author,
    },
}

/// 交给写入线程的入口。克隆的都是同一个线程。
#[derive(Clone)]
pub struct Writer {
    jobs: mpsc::UnboundedSender<(Job, Reply)>,
}

impl Writer {
    /// 起写入线程。`repo` 是上下文仓库的路径，`remote` 是它的远端（没配时为空）。
    pub fn spawn(store: &Store, repo: PathBuf, remote: Option<Remote>) -> Result<Self, Error> {
        let (jobs, mut rx) = mpsc::unbounded_channel::<(Job, Reply)>();
        let main = store.clone();
        std::thread::Builder::new()
            .name("tcms-writer".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("写入线程起不来：{e}");
                        return;
                    }
                };
                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, async move {
                    let mut own: Option<Store> = None;
                    while let Some((job, reply)) = rx.recv().await {
                        if own.is_none() {
                            match main.sibling(3).await {
                                Ok(s) => own = Some(s),
                                Err(e) => eprintln!("写入线程连不上数据库：{e}"),
                            }
                        }
                        let Some(store) = own.clone() else {
                            let _ = reply.send(Err(ApiError::internal("写入线程连不上数据库")));
                            continue;
                        };
                        let repo = repo.clone();
                        let remote = remote.clone();
                        // 单独起一个任务：里面 panic 了只丢这一件，线程接着做下一件。
                        let done = tokio::task::spawn_local(async move {
                            run(job, &store, &repo, remote).await
                        })
                        .await;
                        let _ = reply.send(match done {
                            Ok(r) => r,
                            Err(e) => Err(ApiError::internal(format!("写入时出错：{e}"))),
                        });
                    }
                });
            })
            .map_err(|e| Error::Thread(e.to_string()))?;
        Ok(Self { jobs })
    }

    pub(crate) async fn accept(&self, id: String, opts: AcceptOptions) -> Result<Value, ApiError> {
        self.send(Job::Accept { id, opts }).await
    }

    pub(crate) async fn edit(
        &self,
        id: String,
        base_revision: i32,
        change: ItemEdit,
        actor: String,
        author: Author,
    ) -> Result<Value, ApiError> {
        self.send(Job::Edit {
            id,
            base_revision,
            change,
            actor,
            author,
        })
        .await
    }

    /// 和远端同步一次（拉、索引、推），排在批准、改单条的同一个队里。没配远端时什么都不做。
    /// 合并提交的作者是 `author`。
    pub async fn sync(&self, author: Author) -> Result<RemoteReport, String> {
        let v = self
            .send(Job::Sync { author })
            .await
            .map_err(|e| e.message().to_owned())?;
        serde_json::from_value(v).map_err(|e| format!("同步的结果读不出来：{e}"))
    }

    async fn send(&self, job: Job) -> Result<Value, ApiError> {
        let (reply, rx) = oneshot::channel();
        if self.jobs.send((job, reply)).is_err() {
            return Err(ApiError::internal("写入线程已经停了"));
        }
        rx.await
            .unwrap_or_else(|_| Err(ApiError::internal("写入线程没有回答")))
    }
}

async fn run(
    job: Job,
    store: &Store,
    repo: &std::path::Path,
    remote: Option<Remote>,
) -> Result<Value, ApiError> {
    let repo = ContextRepo::open(repo)
        .map_err(|e| ApiError::internal(format!("打开上下文仓库 {}：{e}", repo.display())))?;
    match job {
        Job::Accept { id, mut opts } => {
            opts.remote = remote;
            let done = tcms_write::accept(&repo, store, &id, &opts)
                .await
                .map_err(write_error)?;
            Ok(json!({
                "proposal": done.proposal,
                "item": done.item,
                "commit": done.commit,
                "path": done.path,
                "already_applied": done.already_applied,
                "remote": done.remote,
            }))
        }
        Job::Edit {
            id,
            base_revision,
            change,
            actor,
            author,
        } => {
            let by = Committer {
                actor,
                author,
                remote,
            };
            let done = tcms_write::edit_item(&repo, store, &id, base_revision, &change, &by)
                .await
                .map_err(write_error)?;
            Ok(json!({
                "item": done.item,
                "revision": done.revision,
                "commit": done.commit,
                "path": done.path,
                "remote": done.remote,
            }))
        }
        Job::Sync { author } => {
            let Some(remote) = remote else {
                return Ok(json!(RemoteReport::default()));
            };
            let report = tcms_write::sync(&repo, store, &remote, &author)
                .await
                .map_err(write_error)?;
            Ok(json!(report))
        }
    }
}

/// 写入出错 → 给网页的回答。
pub(crate) fn write_error(e: WriteError) -> ApiError {
    use http::StatusCode as S;
    let msg = e.to_string();
    match e {
        WriteError::NotFound(_) => ApiError::new(S::NOT_FOUND, "not_found", msg),
        WriteError::NotOpen { .. } => ApiError::new(S::CONFLICT, "not_open", msg),
        WriteError::Stale { current, .. } => {
            ApiError::new(S::CONFLICT, "stale", msg).with_current(current)
        }
        WriteError::TargetGone(_) => ApiError::new(S::CONFLICT, "target_gone", msg),
        WriteError::Invalid(_) | WriteError::Edit(_) => {
            ApiError::new(S::BAD_REQUEST, "invalid", msg)
        }
        WriteError::Git(tcms_git::Error::HeadMoved | tcms_git::Error::Dirty) => {
            ApiError::new(S::CONFLICT, "repo_busy", msg)
        }
        WriteError::Remote(RemoteError::NeedsMerge { .. }) => {
            ApiError::new(S::CONFLICT, "needs_merge", msg)
        }
        WriteError::Remote(_) => {
            eprintln!("写入出错：{msg}");
            ApiError::internal(msg)
        }
        WriteError::Git(_) | WriteError::Index(_) | WriteError::Store(_) => {
            eprintln!("写入出错：{msg}");
            ApiError::internal(msg)
        }
    }
}
