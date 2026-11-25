//! 和 AFFiNE 同步（`docs/affine-sync.md`）本系统这一侧。
//!
//! 读写 AFFiNE 文档结构的代码（A1）2026-11-24 搬进了不依赖本系统的 `affine-doc`
//! （`docs/affine-kernel-rust.md` 的 K1），这里先原样转出去，调用的地方不用改。
//! 连 AFFiNE 的那一层在 `affine-sync-client`（A2）。
//!
//! 同步（A3）：
//! - [`plan`]：一篇文档的三方比较（git、AFFiNE、上次同步完的底稿），算出写到哪边、哪些要人处理；
//! - [`bridge`]：接上范围或文件（[`link`]）、同步一次（[`sync`]）、看状态（[`status`]）、处理冲突（[`resolve`]）；
//! - [`server`]：同步要 AFFiNE 做的几件事（[`Affine`]）和连真的 AFFiNE 的实现（[`Server`]）；
//! - [`follow`]：一直跟着同步（`tcms serve` 里跑），AFFiNE 里改完停一下、git 里一变就同步。
//! - [`comments`]：提议显示成 AFFiNE 评论（A4b）：发评论、记下回复、处理完回复结果并标成已解决。

pub use affine_doc::*;

pub mod bridge;
pub mod comments;
pub mod follow;
pub mod plan;
pub mod server;

pub use bridge::{
    AFFINE_EMAIL, Context, Counts, DocReport, DocStatus, Error, Keep, SYNC_TRAILER, Status,
    SyncReport, folders_doc_id, link, resolve, status, sync,
};
pub use comments::{CommentReport, sync_comments};
pub use follow::{
    Connection, FollowEnv, FollowEvent, FollowLink, FollowOptions, FollowState, Job, Queued,
    Schedule, follow,
};
pub use server::{Affine, Event, Server};
