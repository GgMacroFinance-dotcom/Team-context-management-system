//! MCP 服务端（`docs/spec.md` 6.1，`docs/architecture.md` 第 7 节）。
//!
//! 工具：`context_get`、`context_search`、`context_read`、`context_changes`（`context_propose` 在第 4 步）。
//! 接法：stdio（同机的 agent）和 Streamable HTTP（`/mcp`）。调用者是谁、能看哪些范围，由令牌决定。

mod live;
mod server;
pub mod tools;

pub use live::Live;
pub use server::{ContextServer, HttpOptions, bearer, router, serve_http, serve_stdio};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] tcms_store::Error),
    #[error(transparent)]
    Search(#[from] tcms_search::Error),
    #[error("后台任务出错：{0}")]
    Join(String),
    #[error("MCP 握手失败：{0}")]
    Mcp(String),
}
