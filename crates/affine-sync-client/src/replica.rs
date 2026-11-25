//! 本地副本：一个工作区在本机的 SQLite 库（`affine-store`）加上和服务端的同步（`docs/affine-kernel-rust.md` 的 K4）。
//!
//! - 本地改的先写进库（[`Replica::write`]），不管连没连着；连着的时候 [`Replica::sync_doc`] 马上推。
//! - 连上（第一次连上、断线重连以后、收到“工作区有文档变了”）时 [`Replica::sync_all`]：问服务端每篇文档最后改的时间，
//!   比拉到的新就拉；本地时钟比推过的新就推。
//! - 一篇文档的同步（[`Replica::sync_doc`]）：拿本地的状态向量取服务端缺给本地的那段，记进库；再拿服务端的状态向量算出
//!   服务端缺的那段推上去。两边同时改了也没关系，Yjs 合得起来，谁的字都在。
//! - 服务端推来的别人的修改（[`Replica::apply_event`]）直接记进库。
//!
//! 时钟只在做成了以后才往前走：推、拉到一半断了，下次照样会再做一遍，多推一次一样的更新不会改坏文档。
//! 推上去以后服务端的时间戳会变新，下一次 `sync_all` 会对这篇再取一次差量（是空的），不把它当成拉到了：
//! 推和取之间别人改的，这样也不会漏。
//!
//! 附件（`blobs_to_upload`）还没接，K4 以后再做。

use std::collections::BTreeSet;
use std::future::Future;

use affine_store::Store;

use crate::Error;
use crate::sync::{Intent, LoadedDoc, Pushed, SyncConnection, SyncEvent};

/// 副本要服务端做的三件事。[`SyncConnection`] 实现了它；测试里换成内存里的假服务端。
pub trait Remote {
    fn load_doc(
        &self,
        workspace_id: &str,
        doc_id: &str,
        state_vector: Option<&[u8]>,
    ) -> impl Future<Output = Result<Option<LoadedDoc>, Error>> + Send;

    fn push_update(
        &self,
        workspace_id: &str,
        doc_id: &str,
        update: &[u8],
        intent: Intent,
    ) -> impl Future<Output = Result<Pushed, Error>> + Send;

    fn doc_timestamps(
        &self,
        workspace_id: &str,
        since: Option<i64>,
    ) -> impl Future<Output = Result<std::collections::BTreeMap<String, i64>, Error>> + Send;
}

impl Remote for SyncConnection {
    fn load_doc(
        &self,
        workspace_id: &str,
        doc_id: &str,
        state_vector: Option<&[u8]>,
    ) -> impl Future<Output = Result<Option<LoadedDoc>, Error>> + Send {
        SyncConnection::load_doc(self, workspace_id, doc_id, state_vector)
    }

    fn push_update(
        &self,
        workspace_id: &str,
        doc_id: &str,
        update: &[u8],
        intent: Intent,
    ) -> impl Future<Output = Result<Pushed, Error>> + Send {
        SyncConnection::push_update(self, workspace_id, doc_id, update, intent)
    }

    fn doc_timestamps(
        &self,
        workspace_id: &str,
        since: Option<i64>,
    ) -> impl Future<Output = Result<std::collections::BTreeMap<String, i64>, Error>> + Send {
        SyncConnection::doc_timestamps(self, workspace_id, since)
    }
}

/// 一篇文档同步完的结果。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DocSync {
    /// 从服务端拉到了本地没有的修改。
    pub pulled: bool,
    /// 把服务端没有的修改推上去了。
    pub pushed: bool,
    /// 推的时候服务端还没有这篇，是新建的。
    pub created: bool,
}

/// [`Replica::sync_all`] 的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// 看过的文档（按 id 排）。
    pub checked: Vec<String>,
    pub pulled: Vec<String>,
    pub pushed: Vec<String>,
}

/// 收到一件事以后还要做什么。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FollowUp {
    Nothing,
    /// 拿状态向量重新同步这一篇。
    SyncDoc(String),
    /// 整个工作区看一遍。
    SyncAll,
}

/// 一个工作区的本地副本。
pub struct Replica {
    store: Store,
    workspace_id: String,
    /// 库里记对方时钟用的名字，一个服务端一个，比如服务端的地址。
    peer: String,
}

impl Replica {
    /// 库里记着的工作区 id 和给的不一样时报错（一个库只放一个工作区）；没记过就记下。
    pub fn new(store: Store, workspace_id: &str, peer: &str) -> Result<Self, Error> {
        match store.space_id()? {
            Some(id) if id != workspace_id => {
                return Err(Error::Config(format!(
                    "本地库是工作区 {id} 的，不是 {workspace_id}"
                )));
            }
            Some(_) => {}
            None => store.set_space_id(workspace_id)?,
        }
        Ok(Self {
            store,
            workspace_id: workspace_id.to_owned(),
            peer: peer.to_owned(),
        })
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub fn store(&mut self) -> &mut Store {
        &mut self.store
    }

    /// 本地改了一篇文档：只写进库，返回这篇新的时钟。连着的话接着调 [`Replica::sync_doc`] 推上去。
    pub fn write(&mut self, doc_id: &str, update: &[u8]) -> Result<i64, Error> {
        Ok(self.store.push_update(doc_id, update)?)
    }

    /// 本地这篇文档整篇（v1 更新）。没有时 `None`。
    pub fn read(&mut self, doc_id: &str) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.store.get_doc(doc_id)?.map(|d| d.bin))
    }

    /// 还没推上去的文档。
    pub fn pending(&self) -> Result<Vec<String>, Error> {
        Ok(self
            .store
            .docs_to_push(&self.peer)?
            .into_iter()
            .map(|d| d.doc_id)
            .collect())
    }

    /// 同步一篇文档：先拉服务端有、本地没有的，再推本地有、服务端没有的。
    pub async fn sync_doc(&mut self, remote: &impl Remote, doc_id: &str) -> Result<DocSync, Error> {
        let local = self.store.get_doc(doc_id)?;
        let local_sv = match &local {
            Some(d) => Some(
                yrs::encode_state_vector_from_update_v1(&d.bin)
                    .map_err(|e| Error::Protocol(format!("本地文档 {doc_id} 解不开：{e}")))?,
            ),
            None => None,
        };
        let mut clock = local.as_ref().map(|d| d.clock).unwrap_or(0);
        let mut out = DocSync::default();
        let loaded = remote
            .load_doc(&self.workspace_id, doc_id, local_sv.as_deref())
            .await?;
        match loaded {
            Some(l) => {
                if !is_empty_update(&l.update) {
                    clock = self.store.push_update(doc_id, &l.update)?;
                    out.pulled = true;
                }
                self.store
                    .set_remote_clock(&self.peer, doc_id, l.timestamp)?;
                self.store
                    .set_pulled_remote_clock(&self.peer, doc_id, l.timestamp)?;
                if let Some((diff, _)) = self.store.get_doc_diff(doc_id, Some(&l.state_vector))?
                    && !is_empty_update(&diff)
                {
                    let p = remote
                        .push_update(&self.workspace_id, doc_id, &diff, Intent::Update)
                        .await?;
                    self.store
                        .set_remote_clock(&self.peer, doc_id, p.timestamp)?;
                    out.pushed = true;
                }
            }
            None => {
                if let Some(d) = &local {
                    let p = remote
                        .push_update(&self.workspace_id, doc_id, &d.bin, Intent::Create)
                        .await?;
                    self.store
                        .set_remote_clock(&self.peer, doc_id, p.timestamp)?;
                    out.pushed = true;
                    out.created = true;
                }
            }
        }
        if clock > 0 {
            self.store.set_pushed_clock(&self.peer, doc_id, clock)?;
        }
        Ok(out)
    }

    /// 整个工作区看一遍：服务端改过、还没拉到的拉，本地改过、还没推的推。中途断了返回错，做成了的不会再做。
    pub async fn sync_all(&mut self, remote: &impl Remote) -> Result<SyncReport, Error> {
        let stamps = remote.doc_timestamps(&self.workspace_id, None).await?;
        for (doc_id, t) in &stamps {
            self.store.set_remote_clock(&self.peer, doc_id, *t)?;
        }
        let mut docs: BTreeSet<String> = self
            .store
            .docs_to_pull(&self.peer)?
            .into_iter()
            .map(|d| d.doc_id)
            .collect();
        docs.extend(
            self.store
                .docs_to_push(&self.peer)?
                .into_iter()
                .map(|d| d.doc_id),
        );
        let mut report = SyncReport::default();
        for doc_id in docs {
            let r = self.sync_doc(remote, &doc_id).await?;
            if r.pulled {
                report.pulled.push(doc_id.clone());
            }
            if r.pushed {
                report.pushed.push(doc_id.clone());
            }
            report.checked.push(doc_id);
        }
        Ok(report)
    }

    /// 处理服务端推来的一件事。别人的修改直接记进库；别的告诉调用的一方接着做什么。
    pub fn apply_event(&mut self, event: &SyncEvent) -> Result<FollowUp, Error> {
        match event {
            SyncEvent::DocUpdates {
                workspace_id,
                doc_id,
                updates,
                timestamp,
                ..
            } if *workspace_id == self.workspace_id => {
                for u in updates {
                    match self.store.push_update(doc_id, u) {
                        Ok(_) => {}
                        Err(affine_store::Error::BadUpdate(_)) => {
                            return Ok(FollowUp::SyncDoc(doc_id.clone()));
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                self.store
                    .set_remote_clock(&self.peer, doc_id, *timestamp)?;
                // 收到的是别人改的，自己推的不会广播回来，所以本地这时不缺这一段以前的；
                // 断开期间漏掉的由重连以后的 sync_all 补。
                self.store
                    .set_pulled_remote_clock(&self.peer, doc_id, *timestamp)?;
                Ok(FollowUp::Nothing)
            }
            SyncEvent::DocStale {
                workspace_id,
                doc_id,
            } if *workspace_id == self.workspace_id => Ok(FollowUp::SyncDoc(doc_id.clone())),
            SyncEvent::Invalidated { workspace_id, .. } if *workspace_id == self.workspace_id => {
                Ok(FollowUp::SyncAll)
            }
            SyncEvent::Reconnected => Ok(FollowUp::SyncAll),
            _ => Ok(FollowUp::Nothing),
        }
    }
}

/// 空的更新（什么都没改）：两个字节 0，0。
fn is_empty_update(bin: &[u8]) -> bool {
    bin.is_empty() || bin == [0, 0]
}

#[cfg(test)]
#[path = "replica_tests.rs"]
mod tests;
