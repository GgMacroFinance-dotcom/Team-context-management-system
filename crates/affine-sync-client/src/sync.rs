//! AFFiNE 的同步协议：socket.io 上的 `space:*` 事件（0.27.5 起）。
//!
//! - 加入（`space:join-batch`）：一次最多 100 篇。加入了的文档才能取、才能推修改、才收得到别人改的广播。
//!   工作区的权限变了以后（新建了文档也算）之前的加入作废，取、推被拒时重新加入这一篇再试一次。
//! - 取（`space:load-doc`）：给状态向量就只回缺的那段更新。
//! - 推（`space:push-doc-update`）：一段 Yjs 更新；新建的文档写 `intent: create`，不用先加入。
//! - 哪些文档变过（`space:load-doc-timestamps`）。
//!
//! 协议版本报 1，和 0.27.5 的网页版一样。服务端还有版本 2 才能用的 `space:load-workspace-state`、
//! `space:doc-lifecycle`（服务端记文档列表和回收站），网页版还没用，这里也不用：文档列表和回收站照旧在工作区根文档的
//! `meta.pages` 里。
//! - 服务端推来的：别人对加入了的文档的修改（`space:broadcast-doc-updates`）、工作区的文档列表或权限变了
//!   （`space:broadcast-doc-invalidation`）。
//!
//! 断线以后后台自己重连（1 秒起，每次翻倍，最多 30 秒），重新登录（要的话），重新加入断之前加入了的文档，
//! 再发一个 [`SyncEvent::Reconnected`]：断开的这段时间里的修改收不到广播，用的一方要拿状态向量各取一次差量。
//! 断开的时候调用直接返回 [`Error::Disconnected`]，不排队。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;

use crate::Error;
use crate::client::Client;
use crate::socketio::{self, Incoming, Socket};

const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const JOIN_BATCH: usize = 100;
/// 0.27.5 的网页版用的是 1。
const PROTOCOL_VERSION: u64 = 1;
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// 推一段更新时说明是新建文档还是改已有的。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    Create,
    Update,
}

/// `space:load-doc` 回来的。
#[derive(Clone, Debug)]
pub struct LoadedDoc {
    /// 这边缺的那段更新（没给状态向量时是整篇）。
    pub update: Vec<u8>,
    /// 服务端现在的状态向量。
    pub state_vector: Vec<u8>,
    /// 服务端最后一次改的时间（毫秒）。
    pub timestamp: i64,
}

/// 推上去以后服务端回的。
#[derive(Clone, Copy, Debug)]
pub struct Pushed {
    pub timestamp: i64,
    /// 这次推的是不是新建了文档。
    pub created: bool,
}

/// 服务端推来的事，和连接本身的变化。
#[derive(Clone, Debug)]
pub enum SyncEvent {
    /// 别人（别的连接）改了一篇加入了的文档。
    DocUpdates {
        workspace_id: String,
        doc_id: String,
        updates: Vec<Vec<u8>>,
        timestamp: i64,
        /// 改的人的用户 id。
        editor: Option<String>,
    },
    /// 收到了一篇文档的修改，但解不开：拿状态向量重新取一次差量。
    DocStale {
        workspace_id: String,
        doc_id: String,
    },
    /// 工作区里有文档变了（每次有人推修改都发，也包括没加入的、刚新建的文档）：
    /// 用 [`SyncConnection::doc_timestamps`] 看是哪几篇。
    Invalidated {
        workspace_id: String,
        timestamp: Option<i64>,
    },
    Disconnected {
        reason: String,
    },
    /// 又连上了，断之前加入了的文档都重新加入了。
    Reconnected,
}

/// 一条同步连接。丢掉它连接就关掉，后台也不再重连。
pub struct SyncConnection {
    shared: Arc<Shared>,
    events: mpsc::UnboundedReceiver<SyncEvent>,
    supervisor: JoinHandle<()>,
}

struct Shared {
    client: Client,
    socket: RwLock<Option<Socket>>,
    /// 加入了的：工作区 id → 文档 id（空的表示只加入了工作区）。
    joined: RwLock<BTreeMap<String, BTreeSet<String>>>,
}

impl Client {
    /// 连上同步协议。第一次连不上就返回错；连上以后断了由后台重连。
    pub async fn sync(&self) -> Result<SyncConnection, Error> {
        let shared = Arc::new(Shared {
            client: self.clone(),
            socket: RwLock::new(None),
            joined: RwLock::new(BTreeMap::new()),
        });
        let (socket, incoming) = open(&shared.client).await?;
        *shared.socket.write().await = Some(socket);
        let (tx, events) = mpsc::unbounded_channel();
        let supervisor = tokio::spawn(supervise(shared.clone(), incoming, tx));
        Ok(SyncConnection {
            shared,
            events,
            supervisor,
        })
    }
}

impl Drop for SyncConnection {
    fn drop(&mut self) {
        self.supervisor.abort();
        if let Ok(mut s) = self.shared.socket.try_write() {
            *s = None;
        }
    }
}

impl SyncConnection {
    /// 下一件事。连接丢掉以前不会是 `None`。
    pub async fn next_event(&mut self) -> Option<SyncEvent> {
        self.events.recv().await
    }

    pub async fn is_connected(&self) -> bool {
        self.shared
            .socket
            .read()
            .await
            .as_ref()
            .is_some_and(|s| !s.is_closed())
    }

    /// 加入一个工作区和其中几篇文档（可以是空的，只加入工作区）。已经加入了的不再加。
    pub async fn join(&self, workspace_id: &str, doc_ids: &[&str]) -> Result<(), Error> {
        let new: Vec<String> = {
            let joined = self.shared.joined.read().await;
            match joined.get(workspace_id) {
                Some(docs) => doc_ids
                    .iter()
                    .filter(|d| !docs.contains(**d))
                    .map(|d| (*d).to_owned())
                    .collect(),
                None => doc_ids.iter().map(|d| (*d).to_owned()).collect(),
            }
        };
        let first_time = !self.shared.joined.read().await.contains_key(workspace_id);
        if new.is_empty() && !first_time {
            return Ok(());
        }
        let version = self.shared.client.client_version().await?;
        {
            let guard = self.shared.socket.read().await;
            let socket = guard.as_ref().ok_or(Error::Disconnected)?;
            join_batches(socket, &version, workspace_id, &new).await?;
        }
        self.shared
            .joined
            .write()
            .await
            .entry(workspace_id.to_owned())
            .or_default()
            .extend(new);
        Ok(())
    }

    /// 不再收这几篇文档的广播。
    pub async fn leave(&self, workspace_id: &str, doc_ids: &[&str]) -> Result<(), Error> {
        self.call(
            "space:leave-batch",
            json!({ "spaceType": "workspace", "spaceId": workspace_id, "docIds": doc_ids }),
        )
        .await?;
        if let Some(docs) = self.shared.joined.write().await.get_mut(workspace_id) {
            for d in doc_ids {
                docs.remove(*d);
            }
        }
        Ok(())
    }

    /// 取一篇文档；给了状态向量就只取缺的那段。服务端没有这篇时是 `None`。没加入的先加入。
    pub async fn load_doc(
        &self,
        workspace_id: &str,
        doc_id: &str,
        state_vector: Option<&[u8]>,
    ) -> Result<Option<LoadedDoc>, Error> {
        self.join(workspace_id, &[doc_id]).await?;
        let mut req = json!({ "spaceType": "workspace", "spaceId": workspace_id, "docId": doc_id });
        if let Some(sv) = state_vector {
            req["stateVector"] = json!(BASE64.encode(sv));
        }
        let data = match self.call("space:load-doc", req.clone()).await {
            Err(e) if join_expired(&e) => {
                self.rejoin_doc(workspace_id, doc_id).await?;
                self.call("space:load-doc", req).await
            }
            r => r,
        };
        let data = match data {
            Ok(d) => d,
            Err(Error::Remote { name, .. }) if name == "DOC_NOT_FOUND" => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(Some(LoadedDoc {
            update: b64_field(&data, "missing")?,
            state_vector: b64_field(&data, "state")?,
            timestamp: data
                .get("timestamp")
                .and_then(Value::as_i64)
                .unwrap_or_default(),
        }))
    }

    /// 推一段 Yjs 更新。没加入的先加入：新建时加入工作区就够了，改已有的文档要加入那篇。
    pub async fn push_update(
        &self,
        workspace_id: &str,
        doc_id: &str,
        update: &[u8],
        intent: Intent,
    ) -> Result<Pushed, Error> {
        match intent {
            Intent::Create => self.join(workspace_id, &[]).await?,
            Intent::Update => self.join(workspace_id, &[doc_id]).await?,
        }
        let req = json!({
            "spaceType": "workspace",
            "spaceId": workspace_id,
            "docId": doc_id,
            "update": BASE64.encode(update),
            "intent": match intent { Intent::Create => "create", Intent::Update => "update" },
        });
        let data = match self.call("space:push-doc-update", req.clone()).await {
            Err(e) if intent == Intent::Update && join_expired(&e) => {
                self.rejoin_doc(workspace_id, doc_id).await?;
                self.call("space:push-doc-update", req).await
            }
            r => r,
        }?;
        Ok(Pushed {
            timestamp: data
                .get("timestamp")
                .and_then(Value::as_i64)
                .unwrap_or_default(),
            created: data
                .get("created")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// 工作区里每篇文档最后改的时间（毫秒）；给了 `since` 就只回那以后改过的。
    pub async fn doc_timestamps(
        &self,
        workspace_id: &str,
        since: Option<i64>,
    ) -> Result<BTreeMap<String, i64>, Error> {
        self.join(workspace_id, &[]).await?;
        let mut req = json!({ "spaceType": "workspace", "spaceId": workspace_id });
        if let Some(t) = since {
            req["timestamp"] = json!(t);
        }
        let data = self.call("space:load-doc-timestamps", req).await?;
        let map = data
            .as_object()
            .ok_or_else(|| Error::Protocol(format!("时间戳不是对象：{data}")))?;
        Ok(map
            .iter()
            .filter_map(|(k, v)| v.as_i64().map(|t| (k.clone(), t)))
            .collect())
    }

    /// 断开重连一次（怀疑连接坏了时用；测试也用它）。重连好了会收到 [`SyncEvent::Reconnected`]。
    pub async fn reconnect(&self) {
        if let Some(s) = self.shared.socket.read().await.as_ref() {
            s.close();
        }
    }

    /// 不管加没加入过，再加入一次这一篇（服务端会记下现在的权限）。
    async fn rejoin_doc(&self, workspace_id: &str, doc_id: &str) -> Result<(), Error> {
        let version = self.shared.client.client_version().await?;
        {
            let guard = self.shared.socket.read().await;
            let socket = guard.as_ref().ok_or(Error::Disconnected)?;
            join_batches(socket, &version, workspace_id, &[doc_id.to_owned()]).await?;
        }
        self.shared
            .joined
            .write()
            .await
            .entry(workspace_id.to_owned())
            .or_default()
            .insert(doc_id.to_owned());
        Ok(())
    }

    async fn call(&self, event: &str, data: Value) -> Result<Value, Error> {
        let guard = self.shared.socket.read().await;
        let socket = guard.as_ref().ok_or(Error::Disconnected)?;
        call(socket, event, data).await
    }
}

/// 加入时服务端记下的权限作废了：工作区的权限一变（新建了文档、改了成员……）服务端就换一代，
/// 之前加入的文档再取、再推会被拒（取是 `NOT_IN_SPACE`，推是 `SYNC_PERMISSION_GENERATION_CHANGED`）。
/// 网页版每次取之前都重新加入；这里记着加入过的，被拒了才重新加入这一篇，再试一次。
fn join_expired(e: &Error) -> bool {
    matches!(e, Error::Remote { name, .. }
        if name == "NOT_IN_SPACE" || name == "SYNC_PERMISSION_GENERATION_CHANGED")
}

/// 发一个事件，把回执拆成 `data` 或者错。
async fn call(socket: &Socket, event: &str, data: Value) -> Result<Value, Error> {
    let args = socket.call(event, data, CALL_TIMEOUT).await?;
    let reply = args.into_iter().next().unwrap_or(Value::Null);
    if let Some(err) = reply.get("error") {
        let field = |k: &str| {
            err.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let name = field("name");
        return Err(if name == "AUTHENTICATION_REQUIRED" {
            Error::Auth(field("message"))
        } else {
            Error::Remote {
                name,
                message: field("message"),
            }
        });
    }
    Ok(reply.get("data").cloned().unwrap_or(Value::Null))
}

async fn join_batches(
    socket: &Socket,
    version: &str,
    workspace_id: &str,
    docs: &[String],
) -> Result<(), Error> {
    let entry = |doc: Option<&String>| match doc {
        Some(d) => json!({ "spaceType": "workspace", "spaceId": workspace_id, "docId": d }),
        None => json!({ "spaceType": "workspace", "spaceId": workspace_id }),
    };
    let batches: Vec<Vec<Value>> = if docs.is_empty() {
        vec![vec![entry(None)]]
    } else {
        docs.chunks(JOIN_BATCH)
            .map(|c| c.iter().map(|d| entry(Some(d))).collect())
            .collect()
    };
    for spaces in batches {
        let data = call(
            socket,
            "space:join-batch",
            json!({ "spaces": spaces, "clientVersion": version, "protocolVersion": PROTOCOL_VERSION }),
        )
        .await?;
        if data.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(Error::Remote {
                name: "JOIN_REJECTED".into(),
                message: format!(
                    "服务端不让加入工作区（客户端版本 {version}，0.27.5 以前的服务端不支持批量加入）"
                ),
            });
        }
    }
    Ok(())
}

fn b64_field(data: &Value, key: &str) -> Result<Vec<u8>, Error> {
    let s = data
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Protocol(format!("回复里没有 {key}")))?;
    BASE64
        .decode(s)
        .map_err(|e| Error::Protocol(format!("{key} 不是 base64：{e}")))
}

/// 登录（要的话）并连上 socket.io。会话过期被拒时换个新会话再试一次。
async fn open(client: &Client) -> Result<(Socket, mpsc::UnboundedReceiver<Incoming>), Error> {
    let mut retried = false;
    loop {
        let session = client.session().await?;
        let headers = client.auth_headers(&session).await?;
        match socketio::connect(client.endpoint(), client.tls(), &headers, &json!({})).await {
            Err(Error::Auth(_)) if !retried => {
                client.drop_session(&session).await;
                retried = true;
            }
            r => return r,
        }
    }
}

async fn supervise(
    shared: Arc<Shared>,
    mut incoming: mpsc::UnboundedReceiver<Incoming>,
    tx: mpsc::UnboundedSender<SyncEvent>,
) {
    loop {
        let reason = loop {
            match incoming.recv().await {
                Some(Incoming::Event { name, args }) => {
                    if let Some(ev) = translate(&name, args)
                        && tx.send(ev).is_err()
                    {
                        return;
                    }
                }
                Some(Incoming::Closed(reason)) => break reason,
                None => break "连接没了".to_owned(),
            }
        };
        *shared.socket.write().await = None;
        if tx.send(SyncEvent::Disconnected { reason }).is_err() {
            return;
        }
        let mut delay = RECONNECT_MIN;
        incoming = loop {
            tokio::time::sleep(delay).await;
            if tx.is_closed() {
                return;
            }
            delay = (delay * 2).min(RECONNECT_MAX);
            let Ok((socket, inc)) = open(&shared.client).await else {
                continue;
            };
            if rejoin(&shared, &socket).await.is_err() {
                continue;
            }
            *shared.socket.write().await = Some(socket);
            break inc;
        };
        if tx.send(SyncEvent::Reconnected).is_err() {
            return;
        }
    }
}

async fn rejoin(shared: &Shared, socket: &Socket) -> Result<(), Error> {
    let version = shared.client.client_version().await?;
    let joined = shared.joined.read().await.clone();
    for (workspace, docs) in joined {
        let docs: Vec<String> = docs.into_iter().collect();
        join_batches(socket, &version, &workspace, &docs).await?;
    }
    Ok(())
}

/// 服务端推来的事件变成 [`SyncEvent`]；不关心的（比如别人的光标）丢掉。
fn translate(name: &str, args: Vec<Value>) -> Option<SyncEvent> {
    let msg = args.into_iter().next()?;
    let field = |k: &str| msg.get(k).and_then(Value::as_str).map(str::to_owned);
    if field("spaceType").as_deref() != Some("workspace") {
        return None;
    }
    let workspace_id = field("spaceId")?;
    match name {
        "space:broadcast-doc-updates" => {
            let doc_id = field("docId")?;
            let stale = || SyncEvent::DocStale {
                workspace_id: workspace_id.clone(),
                doc_id: doc_id.clone(),
            };
            // `compressed: true` 只是说服务端把几段更新合成了一段，还是普通的 Yjs 更新。
            let updates: Option<Vec<Vec<u8>>> = msg
                .get("updates")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|u| u.as_str().and_then(|s| BASE64.decode(s).ok()))
                        .collect()
                })
                .unwrap_or(None);
            Some(match updates {
                Some(updates) => SyncEvent::DocUpdates {
                    workspace_id,
                    doc_id,
                    updates,
                    timestamp: msg
                        .get("timestamp")
                        .and_then(Value::as_i64)
                        .unwrap_or_default(),
                    editor: field("editor"),
                },
                None => stale(),
            })
        }
        "space:broadcast-doc-invalidation" => Some(SyncEvent::Invalidated {
            workspace_id,
            timestamp: msg.get("timestamp").and_then(Value::as_i64),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcasts_are_translated() {
        let ev = translate(
            "space:broadcast-doc-updates",
            vec![json!({
                "spaceType": "workspace", "spaceId": "w", "docId": "d",
                "updates": [BASE64.encode([1u8, 2, 3])], "timestamp": 5, "editor": "u"
            })],
        );
        match ev {
            Some(SyncEvent::DocUpdates {
                workspace_id,
                doc_id,
                updates,
                timestamp,
                editor,
            }) => {
                assert_eq!((workspace_id.as_str(), doc_id.as_str()), ("w", "d"));
                assert_eq!(updates, vec![vec![1, 2, 3]]);
                assert_eq!(timestamp, 5);
                assert_eq!(editor.as_deref(), Some("u"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            translate(
                "space:broadcast-doc-updates",
                vec![json!({"spaceType": "workspace", "spaceId": "w", "docId": "d", "updates": ["AQ=="], "compressed": true})]
            ),
            Some(SyncEvent::DocUpdates { updates, .. }) if updates == vec![vec![1u8]]
        ));
        assert!(matches!(
            translate(
                "space:broadcast-doc-updates",
                vec![
                    json!({"spaceType": "workspace", "spaceId": "w", "docId": "d", "updates": ["不是base64"]})
                ]
            ),
            Some(SyncEvent::DocStale { .. })
        ));
        assert!(matches!(
            translate(
                "space:broadcast-doc-invalidation",
                vec![json!({"spaceType": "workspace", "spaceId": "w", "timestamp": 1})]
            ),
            Some(SyncEvent::Invalidated {
                timestamp: Some(1),
                ..
            })
        ));
        assert!(
            translate(
                "space:broadcast-awareness-update",
                vec![json!({"spaceType": "workspace", "spaceId": "w"})]
            )
            .is_none()
        );
        assert!(
            translate(
                "space:broadcast-doc-updates",
                vec![
                    json!({"spaceType": "userspace", "spaceId": "u", "docId": "d", "updates": []})
                ]
            )
            .is_none()
        );
    }
}
