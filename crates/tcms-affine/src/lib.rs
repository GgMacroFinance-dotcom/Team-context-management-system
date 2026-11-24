//! 和 AFFiNE 同步的第一步（`docs/affine-sync.md` 第 7 节 A1）：用 yrs 读写 AFFiNE 的文档结构，
//! 按二级标题块切成条目，Markdown 子集和块互相转换。
//!
//! 只处理一篇文档的 Yjs 内容，不连网络、不碰上下文仓库和数据库：连 AFFiNE 是 A2，
//! 同步循环是 A3。依据是 AFFiNE 0.27.5 的文档结构（只读了 MIT 部分的设计，没复制代码）。

pub mod doc;
pub mod markdown;
pub mod model;
pub mod sections;

pub use doc::{
    Error, Ids, Page, RandomIds, create, insert_item, read, remove_item, set_title, update_item,
};
pub use markdown::{roundtrips, same_content, to_blocks, to_markdown};
pub use model::{Block, Content, ListKind, Marks, ParagraphKind, Span, plain_text};
pub use sections::{Item, Section, split};

#[cfg(test)]
mod tests;
