//! 不带界面的 AFFiNE 文档内核（`docs/affine-kernel-rust.md`，范围甲）：用 yrs 读写 AFFiNE 的
//! 文档和工作区结构，Markdown 子集和块互相转换。
//!
//! 不连网络、不碰数据库，也不依赖 TCMS 的别的 crate，以后能单独发布。连 AFFiNE 服务端的那一层
//! 在 TCMS 的 A2 里写，以后搬进 `affine-sync-client`。依据是 AFFiNE 0.27.5 的结构，只读了 MIT
//! 部分的设计，没复制代码。
//!
//! - [`doc`]：一篇文档（块树）的读写；
//! - [`markdown`]：Markdown 子集和块互相转换；
//! - [`sections`]：按二级标题块切成一条一条；
//! - [`snapshot`]：BlockSuite 的文档快照（`.snapshot.json`）和 Yjs 文档互相转换；
//! - [`workspace`]：根文档里的文档列表、文件夹树、文档属性这几张表；
//! - [`schema`]：全部块的种类、版本、能放在哪里；不挑块的读法和结构检查；
//! - [`order`]：AFFiNE 列表用的排序键。

pub mod doc;
pub mod markdown;
pub mod model;
pub mod order;
pub mod schema;
pub mod sections;
pub mod snapshot;
pub mod workspace;

pub use doc::{
    Error, Ids, Page, RandomIds, create, insert_item, read, remove_item, set_title, update_item,
};
pub use markdown::{roundtrips, same_content, to_blocks, to_markdown};
pub use model::{Block, Content, ListKind, Marks, ParagraphKind, Span, plain_text};
pub use sections::{Item, Section, split};

#[cfg(test)]
mod schema_tests;
#[cfg(test)]
mod snapshot_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod workspace_tests;
