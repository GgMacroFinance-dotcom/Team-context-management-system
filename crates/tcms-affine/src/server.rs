//! 同步程序要 AFFiNE 做的几件事（[`Affine`]），和连真的 AFFiNE 的实现（[`Server`]，用 A2 的连接层）。
//! 测试里换成内存里的假 AFFiNE。

use std::collections::HashMap;

use affine_sync_client::{Client, Config, Error as ClientError, Intent, SyncConnection};
use serde_json::json;
use yrs::{Doc, ReadTxn, StateVector, Transact};

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
}
