//! `tcms bench`：性能基准（`docs/architecture.md` 第 10 节）。除了 `affine-doc`，都要 TCMS_DATABASE_URL，
//! 在里面建一个临时 schema，跑完删掉。

mod affine;
mod affine_sync;
mod data;
mod index;
mod read;
mod serve;

use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use clap::Subcommand;
use tcms_core::ItemId;

#[derive(Subcommand)]
pub enum What {
    /// 索引：全量、没有变更、改一个文件后的增量。
    Index {
        /// 造多少条。
        #[arg(long, default_value_t = 10_000)]
        items: usize,
        /// 超过门槛时以非零状态退出（CI 用）。门槛按 1 万条定，条数不同时按比例换算。
        #[arg(long)]
        check: bool,
    },
    /// 读：取条目、建检索索引、编上下文包、检索。
    Read {
        /// 造多少条。
        #[arg(long, default_value_t = 10_000)]
        items: usize,
        /// 编包和检索各跑多少次。
        #[arg(long, default_value_t = 300)]
        requests: usize,
        /// 每个包的 token 预算。
        #[arg(long, default_value_t = 8000)]
        budget: usize,
        /// 超过门槛时以非零状态退出（CI 用）。
        #[arg(long)]
        check: bool,
    },
    /// 服务：起一个真的 `tcms serve --stdio` 子进程，用 MCP 客户端量往返、令牌核对、换上新索引的时间和空闲时的内存，
    /// 再量提议的往返和批准一条的时间。
    Serve {
        /// 造多少条。
        #[arg(long, default_value_t = 10_000)]
        items: usize,
        /// context_get、context_search、令牌核对、context_propose 各跑多少次。
        #[arg(long, default_value_t = 300)]
        requests: usize,
        /// 每个包的 token 预算。
        #[arg(long, default_value_t = 8000)]
        budget: usize,
        /// 超过门槛时以非零状态退出（CI 用）。
        #[arg(long)]
        check: bool,
    },
    /// 和 AFFiNE 同步：第一次接上、全部看一遍、跟着同步时两边改一条各要多久、跟着同步多用的内存、
    /// 提议显示成评论。AFFiNE 换成这个进程里的假 AFFiNE，`--real` 时连真的。
    Affine {
        /// 造多少条（每个文件 20 条，一个文件一篇文档）。
        #[arg(long, default_value_t = 1000)]
        items: usize,
        /// 跟着同步时 AFFiNE 里、git 里各改几次。
        #[arg(long, default_value_t = 30)]
        edits: usize,
        /// 超过门槛时以非零状态退出（CI 用）。门槛按 1 千条定，第一次接上、全部看一遍按条数比例换算。
        #[arg(long)]
        check: bool,
        /// 连真的 AFFiNE（TCMS_AFFINE_URL、TCMS_AFFINE_EMAIL、TCMS_AFFINE_PASSWORD）。每跑一次在里面新建一个
        /// 工作区，不删，只该对着测试用的 AFFiNE 跑。
        #[arg(long)]
        real: bool,
    },
    /// AFFiNE 内核：读一篇 1 千个块的文档、合并 1 万条更新（含写进本地库）、导出整个工作区。不要数据库。
    AffineDoc {
        /// 超过门槛时以非零状态退出（CI 用）。
        #[arg(long)]
        check: bool,
    },
}

pub async fn run(what: What) -> Result<ExitCode> {
    match what {
        What::Index { items, check } => index::run(items, check).await,
        What::Read {
            items,
            requests,
            budget,
            check,
        } => read::run(items, requests, budget, check).await,
        What::Serve {
            items,
            requests,
            budget,
            check,
        } => serve::run(items, requests, budget, check).await,
        What::Affine {
            items,
            edits,
            check,
            real,
        } => affine_sync::run(items, edits, check, real).await,
        What::AffineDoc { check } => affine::run(check),
    }
}

const ITEMS_PER_FILE: usize = 20;

/// 第 `file` 个文件放在哪个范围：org、5 个团队、10 个岗位、3 个项目轮着来。
fn scope_dir(file: usize) -> String {
    match file % 4 {
        0 => "org".to_owned(),
        1 => format!("team-t{}", file % 5),
        2 => format!("role-r{}", file % 10),
        _ => format!("project-p{}", file % 3),
    }
}

fn temp_schema() -> String {
    format!(
        "tcms_bench_{}",
        ItemId::generate().as_str()[4..].to_lowercase()
    )
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

struct Percentiles {
    p50: Duration,
    p95: Duration,
}

fn percentiles(mut d: Vec<Duration>) -> Percentiles {
    d.sort();
    let at = |q: f64| d[((d.len() as f64 * q).ceil() as usize).clamp(1, d.len()) - 1];
    Percentiles {
        p50: at(0.5),
        p95: at(0.95),
    }
}

/// 进程占用的物理内存（KB），`None` 是自己。只在 Linux 上有（读 /proc/<pid>/status）。
fn rss_kb(pid: Option<u32>) -> Option<u64> {
    let path = match pid {
        Some(p) => format!("/proc/{p}/status"),
        None => "/proc/self/status".to_owned(),
    };
    let status = std::fs::read_to_string(path).ok()?;
    status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}
