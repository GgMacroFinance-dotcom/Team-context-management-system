//! 和 AFFiNE 同步（`docs/affine-sync.md`）本系统这一侧。
//!
//! 读写 AFFiNE 文档结构的代码（A1）2026-11-24 搬进了不依赖本系统的 `affine-doc`
//! （`docs/affine-kernel-rust.md` 的 K1），这里先原样转出去，调用的地方不用改。
//! 同步循环（A3）以后写在这里。

pub use affine_doc::*;
