//! 同步程序要 AFFiNE 做的几件事（[`Affine`]），和连真的 AFFiNE 的实现（[`Server`]，用 A2 的连接层）。
//! 测试里换成内存里的假 AFFiNE。

use std::collections::HashMap;

use affine_sync_client::{
    Client, Comment, Config, DocMode, Error as ClientError, Intent, SyncConnection, SyncEvent,
};
use serde_json::{Value, json};
use yrs::{Doc, ReadTxn, StateVector, Transact};

/// AFFiNE 那边推来的事（一直跟着同步时用，[`crate::follow`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// 别人改了一篇文档（工作区根文档、文件夹树也算）。
    Changed { workspace: String, doc: String },
    /// 连接断了，后台在重连。
    Disconnected(String),
    /// 又连上了：断开的这段时间里的修改收不到广播，要全部看一遍。
    Reconnected,
}

/// 同步要用到的 AFFiNE 接口。
#[allow(async_fn_in_trait)]
pub trait Affine {
    /// 取一篇文档：给了本地的状态向量（v1 编码）就只回缺的那段，没给是整篇。服务端没有这篇时是 `None`。
    async fn load(
        &mut self,
        workspace: &str,
        doc: &str,
        state_vector: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, ClientError>;

    /// 推一段更新；`create` 是新建这篇文档。
    async fn push(
        &mut self,
        workspace: &str,
        doc: &str,
        update: &[u8],
        create: bool,
    ) -> Result<(), ClientError>;

    /// 最近改过的文档是谁最后改的：文档 id → 这个人在 AFFiNE 里的名字。拿不到的不在里面。
    async fn editors(&mut self, workspace: &str) -> Result<HashMap<String, String>, ClientError>;

    /// 一篇文档上的评论，带回复（提议显示成评论，[`crate::comments`]）。
    async fn comments(&mut self, workspace: &str, doc: &str) -> Result<Vec<Comment>, ClientError>;

    /// 在一篇文档上发一条评论（`content` 用 `affine_sync_client::comment_spans` 做），返回评论的 id。
    async fn create_comment(
        &mut self,
        workspace: &str,
        doc: &str,
        doc_title: &str,
        content: Value,
    ) -> Result<String, ClientError>;

    /// 回复一条评论。
    async fn reply(
        &mut self,
        comment_id: &str,
        doc_title: &str,
        content: Value,
    ) -> Result<(), ClientError>;

    /// 把评论标成已解决。
    async fn resolve_comment(&mut self, comment_id: &str) -> Result<(), ClientError>;

    /// 要收这几篇文档的修改（取过的已经在收了；新建的要说一下）。
    async fn watch(&mut self, workspace: &str, docs: &[String]) -> Result<(), ClientError> {
        let _ = (workspace, docs);
        Ok(())
    }

    /// 下一件事。推不来事的实现一直等着。
    async fn next_event(&mut self) -> Event {
        std::future::pending().await
    }
}

/// 连着的 AFFiNE。
pub struct Server {
    client: Client,
    conn: SyncConnection,
}

impl Server {
    /// 登录、连上同步协议。
    pub async fn connect(config: Config) -> Result<Self, ClientError> {
        let client = Client::new(config)?;
        let conn = client.sync().await?;
        Ok(Self { client, conn })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// 新建一个工作区，起名 `name`，返回它的 id。
    pub async fn create_workspace(&mut self, name: &str) -> Result<String, ClientError> {
        let ws = self.client.create_workspace().await?;
        let root = Doc::new();
        affine_doc::workspace::set_workspace_name(&root, name);
        let full = root
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        self.conn
            .push_update(&ws, &ws, &full, Intent::Create)
            .await?;
        Ok(ws)
    }
}

impl Affine for Server {
    async fn load(
        &mut self,
        workspace: &str,
        doc: &str,
        state_vector: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, ClientError> {
        Ok(self
            .conn
            .load_doc(workspace, doc, state_vector)
            .await?
            .map(|d| d.update))
    }

    async fn push(
        &mut self,
        workspace: &str,
        doc: &str,
        update: &[u8],
        create: bool,
    ) -> Result<(), ClientError> {
        let intent = if create {
            Intent::Create
        } else {
            Intent::Update
        };
        self.conn
            .push_update(workspace, doc, update, intent)
            .await?;
        Ok(())
    }

    async fn editors(&mut self, workspace: &str) -> Result<HashMap<String, String>, ClientError> {
        let recent = self
            .client
            .recently_updated_docs(workspace, 100, None)
            .await?;
        let members = self
            .client
            .graphql(
                "query($id: String!) { workspace(id: $id) { members(skip: 0, take: 200) { id name } } }",
                json!({ "id": workspace }),
            )
            .await?;
        let names: HashMap<&str, &str> = members["workspace"]["members"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| Some((m["id"].as_str()?, m["name"].as_str()?)))
            .collect();
        Ok(recent
            .items
            .into_iter()
            .filter_map(|d| {
                let name = names.get(d.last_updater_id.as_deref()?)?;
                Some((d.id, (*name).to_owned()))
            })
            .collect())
    }

    async fn comments(&mut self, workspace: &str, doc: &str) -> Result<Vec<Comment>, ClientError> {
        self.client.comments(workspace, doc).await
    }

    async fn create_comment(
        &mut self,
        workspace: &str,
        doc: &str,
        doc_title: &str,
        content: Value,
    ) -> Result<String, ClientError> {
        let c = self
            .client
            .create_comment(workspace, doc, doc_title, DocMode::Page, content)
            .await?;
        Ok(c.id)
    }

    async fn reply(
        &mut self,
        comment_id: &str,
        doc_title: &str,
        content: Value,
    ) -> Result<(), ClientError> {
        self.client
            .create_reply(comment_id, doc_title, DocMode::Page, content)
            .await?;
        Ok(())
    }

    async fn resolve_comment(&mut self, comment_id: &str) -> Result<(), ClientError> {
        self.client.resolve_comment(comment_id, true).await
    }

    async fn watch(&mut self, workspace: &str, docs: &[String]) -> Result<(), ClientError> {
        let ids: Vec<&str> = docs.iter().map(String::as_str).collect();
        self.conn.join(workspace, &ids).await
    }

    async fn next_event(&mut self) -> Event {
        loop {
            match self.conn.next_event().await {
                Some(
                    SyncEvent::DocUpdates {
                        workspace_id,
                        doc_id,
                        ..
                    }
                    | SyncEvent::DocStale {
                        workspace_id,
                        doc_id,
                    },
                ) => {
                    return Event::Changed {
                        workspace: workspace_id,
                        doc: doc_id,
                    };
                }
                Some(SyncEvent::Disconnected { reason }) => return Event::Disconnected(reason),
                Some(SyncEvent::Reconnected) => return Event::Reconnected,
                // 工作区里有文档变了（自己推的也发）：加入了的文档另有广播，没加入的新文档会改到文件夹树，这里不用。
                Some(SyncEvent::Invalidated { .. }) => {}
                // 连接整个没了（后台的重连也退出了）：不会再有事。
                None => std::future::pending::<()>().await,
            }
        }
    }
}
