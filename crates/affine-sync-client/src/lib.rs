//! 和 AFFiNE 的连接层（`docs/affine-sync.md` 第 7 节 A2）。
//!
//! - [`Client`]：一个 AFFiNE 服务和一个账号。邮箱密码登录，拿网页版那种 cookie 会话；GraphQL（工作区、
//!   最近改过的文档、评论）。
//! - [`SyncConnection`]：socket.io 上的同步协议，收发的是 Yjs 更新的字节；断线以后自己重连。
//! - [`Replica`]：一个工作区的本地副本（`affine-store` 的 SQLite 库），断网时照样能改，连上以后补推、补拉。
//!
//! 这个 crate 只认 AFFiNE 的协议，不用 TCMS 自己的类型。2026-11-24 从 TCMS 的 `tcms-affine-client`（A2）原样改名搬进
//! AFFiNE 内核（`docs/affine-kernel-rust.md` 的 K3）。文档里面是什么（块、条目、文件夹表）由 `affine-doc` 管。
//!
//! 按 AFFiNE 0.27.5 的协议写（0.27.5 的正式镜像还没出，对着 `0.27.5-beta.3` 测过）；这是 AFFiNE 客户端自己用的协议，
//! 没有承诺对外稳定，跟着 AFFiNE 的版本走。

pub mod client;
pub mod graphql;
mod http;
pub mod net;
pub mod replica;
mod socketio;
pub mod sync;

pub use client::{Client, Config, Credentials, ServerInfo};
pub use graphql::{
    Comment, CommentChange, DocInfo, DocMode, Page, WorkspaceInfo, comment_content, comment_text,
};
pub use net::Endpoint;
pub use replica::{DocSync, FollowUp, Remote, Replica, SyncReport};
pub use sync::{Intent, LoadedDoc, Pushed, SyncConnection, SyncEvent};

/// 连 AFFiNE 时出的错。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("设置不对：{0}")]
    Config(String),
    #[error("网络：{0}")]
    Network(String),
    #[error("登录不上：{0}")]
    Auth(String),
    /// AFFiNE 回的错，`name` 是它的错误名（比如 `DOC_NOT_FOUND`、`SPACE_ACCESS_DENIED`）。
    #[error("AFFiNE 说 {name}：{message}")]
    Remote { name: String, message: String },
    #[error("和 AFFiNE 的协议对不上：{0}")]
    Protocol(String),
    #[error("和 AFFiNE 的连接断了")]
    Disconnected,
    #[error("等 AFFiNE 回复 {0} 超时")]
    Timeout(String),
    /// 本地副本的库（[`Replica`]）出的错。
    #[error("本地库：{0}")]
    Store(#[from] affine_store::Error),
}

impl Error {
    /// 断线、超时、连不上、请求太多这类过一会儿再试可能就好的。
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Network(_) | Self::Disconnected | Self::Timeout(_) => true,
            Self::Remote { name, .. } => name == "TOO_MANY_REQUEST",
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests;
