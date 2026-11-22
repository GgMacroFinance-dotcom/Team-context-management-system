//! 检索（`docs/architecture.md` 第 6 节）：中文按相邻两字切分，tantivy 内存索引，BM25 加确定性排序。
//!
//! [`Corpus`] 是从 PostgreSQL 取出的一份生效条目，[`SearchIndex`] 在它上面建全文索引，
//! 上下文包（`tcms-pack`）和 `context_search` 都用它。

mod corpus;
mod index;
mod tokenize;

pub use corpus::{Corpus, CorpusItem, Visibility};
pub use index::{Hit, MAX_LIMIT, MAX_QUERY_TERMS, SearchIndex, SearchOptions, excerpt};
pub use tokenize::{Piece, TOKENIZER, TcmsTokenizer, distinct_terms, is_cjk, tokenize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("检索索引：{0}")]
    Tantivy(#[from] tantivy::TantivyError),
    #[error(transparent)]
    Store(#[from] tcms_store::Error),
    #[error("索引里的条目读不懂：{0}")]
    BadRow(String),
    #[error("名字不对：{0}（只能含字母、数字、-、_）")]
    BadName(String),
}
