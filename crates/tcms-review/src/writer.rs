//! 写 git 的那个线程：批准提议、改单条、和远端同步。
//!
//! gix 的仓库句柄不能在线程之间共享，批准的过程又要跨好几次等数据库，所以放在一个专门的线程里，
//! 自己的单线程 tokio 运行时、自己的连接池（和服务的提交队列锁是同一把），一次做一件。
//! 每件事都重新打开上下文仓库，读到的总是磁盘上最新的样子。配了远端时，批准、改单条先拉后推（`tcms_write`）。
//! 每次和远端打交道的结果记在 [`RemoteStatus`] 里，审核页（`GET /api/remote`）拿去显示。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
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

/// 和远端打交道的情况，给审核页看。只记在内存里，服务重启以后从头记。
#[derive(Debug, Clone, Default, Serialize)]
pub struct RemoteStatus {
    /// 远端（地址里的用户名、密码换成了 `***`）；没配远端时为空。
    pub remote: Option<String>,
    /// 定时同步的间隔（秒），0 是不定时同步（只在批准、改单条时拉推）。
    pub sync_secs: u64,
    /// 最近一次和远端打交道（定时同步、批准、改单条）。
    pub last: Option<Contact>,
    /// 最近一次拉、推都没出错的时间（Unix 毫秒）。
    pub last_ok_ms: Option<u64>,
    /// 合并不了时，哪些文件、怎么合并（和日志里的一样）。之后拉成功了就清掉。
    pub needs_merge: Option<String>,
}

/// 一次和远端打交道。
#[derive(Debug, Clone, Serialize)]
pub struct Contact {
    /// 什么时候（Unix 毫秒）。
    pub at_ms: u64,
    /// `sync`（定时同步）、`accept`（批准）、`edit`（改单条）。
    pub what: &'static str,
    pub report: RemoteReport,
}

impl RemoteStatus {
    fn record(&mut self, what: &'static str, report: &RemoteReport) {
        let now = now_ms();
        if report.errors.is_empty() {
            self.last_ok_ms = Some(now);
        }
        if report.needs_merge {
            self.needs_merge = report
                .errors
                .iter()
                .find_map(|e| e.strip_prefix("拉取："))
                .or(report.errors.first().map(String::as_str))
                .map(str::to_owned);
        } else if report.pulled.is_some() {
            self.needs_merge = None;
        }
        self.last = Some(Contact {
            at_ms: now,
            what,
            report: report.clone(),
        });
    }

    /// 批准、改单条时合并不了（这时没有 [`RemoteReport`]，只有错误）。
    fn record_needs_merge(&mut self, what: &'static str, message: String) {
        let report = RemoteReport {
            remote: self.remote.clone().unwrap_or_default(),
            needs_merge: true,
            errors: vec![format!("拉取：{message}")],
            ..RemoteReport::default()
        };
        self.record(what, &report);
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

type Status = Arc<Mutex<RemoteStatus>>;

fn lock(status: &Status) -> std::sync::MutexGuard<'_, RemoteStatus> {
    status.lock().unwrap_or_else(|e| e.into_inner())
}

/// 交给写入线程的入口。克隆的都是同一个线程。
#[derive(Clone)]
pub struct Writer {
    jobs: mpsc::UnboundedSender<(Job, Reply)>,
    status: Status,
    /// 上下文仓库的路径（审核页查 `teams.yaml` 时只读）。
    repo: PathBuf,
}

impl Writer {
    /// 起写入线程。`repo` 是上下文仓库的路径，`remote` 是它的远端（没配时为空），
    /// `sync_every` 是服务多久和远端同步一次（只是给审核页显示，定时同步本身不在这里）。
    pub fn spawn(
        store: &Store,
        repo: PathBuf,
        remote: Option<Remote>,
        sync_every: Duration,
    ) -> Result<Self, Error> {
        let (jobs, mut rx) = mpsc::unbounded_channel::<(Job, Reply)>();
        let main = store.clone();
        let status: Status = Arc::new(Mutex::new(RemoteStatus {
            remote: remote.as_ref().map(Remote::display),
            sync_secs: sync_every.as_secs(),
            ..RemoteStatus::default()
        }));
        let shared = status.clone();
        let repo_path = repo.clone();
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
                        let status = shared.clone();
                        // 单独起一个任务：里面 panic 了只丢这一件，线程接着做下一件。
                        let done = tokio::task::spawn_local(async move {
                            run(job, &store, &repo, remote, &status).await
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
        Ok(Self {
            jobs,
            status,
            repo: repo_path,
        })
    }

    pub(crate) fn repo(&self) -> &std::path::Path {
        &self.repo
    }

    /// 和远端打交道的情况（审核页显示）。
    pub fn remote_status(&self) -> RemoteStatus {
        lock(&self.status).clone()
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
    status: &Status,
) -> Result<Value, ApiError> {
    let repo = ContextRepo::open(repo)
        .map_err(|e| ApiError::internal(format!("打开上下文仓库 {}：{e}", repo.display())))?;
    // 合并不了时记下来，审核页顶上提示。
    let noted = |what: &'static str| {
        move |e: WriteError| {
            if let WriteError::Remote(m @ RemoteError::NeedsMerge { .. }) = &e {
                lock(status).record_needs_merge(what, m.to_string());
            }
            write_error(e)
        }
    };
    match job {
        Job::Accept { id, mut opts } => {
            opts.remote = remote;
            let done = tcms_write::accept(&repo, store, &id, &opts)
                .await
                .map_err(noted("accept"))?;
            if let Some(r) = &done.remote {
                lock(status).record("accept", r);
            }
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
                .map_err(noted("edit"))?;
            if let Some(r) = &done.remote {
                lock(status).record("edit", r);
            }
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
            lock(status).record("sync", &report);
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
