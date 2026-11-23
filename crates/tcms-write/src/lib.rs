//! 写（`docs/spec.md` 第 5 节，`docs/architecture.md` 第 3 节）。
//!
//! - [`propose`]：agent 的提议。检查（范围、类型、规则不能提）、脱敏、找相近的条目、挡乱提，存进 PostgreSQL，不动 git。
//! - [`accept`]、[`reject`]：人批准或拒绝。批准时排队（一个 schema 同一时间只处理一条），先把索引带到 HEAD，
//!   条目在提议之后被人改过就停下来要人看过，再改文件、提交、索引。
//! - [`proposal_diff`]：给审核的人看批准以后差在哪里。
//! - [`edit_item`]：人在审核页里改单条的纯文本，和批准排在同一个提交队列里。
//! - [`sync`]：和上下文仓库的远端同步（拉、索引、推）。批准和改单条配了远端时也会先拉、后推。
//!   拉下来的新条目没有 id 时补上（[`assign_since`]）。
//! - [`assign_all`]：`tcms assign-ids`，给整个仓库里没有 id 的条目补 id。

mod accept;
mod conflicts;
mod edit_item;
mod ids;
mod propose;
mod review;
mod sync;

pub use accept::{AcceptOptions, Accepted, WriteError, accept, default_path, reject};
pub use conflicts::{Conflict, find_conflicts};
pub use edit_item::{Committer, Edited, ItemEdit, edit_item};
pub use ids::{AssignedId, IdsAssigned, assign_all, assign_since};
pub use propose::{
    Draft, MAX_OPEN_PER_AGENT, Proposal, ProposeArgs, ProposeError, item_id_of, propose, reach,
};
pub use review::{DiffLine, diff, item_text, proposal_diff};
pub use sync::{RemoteReport, sync};
