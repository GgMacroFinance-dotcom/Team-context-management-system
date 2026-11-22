//! `tcms bench`：性能基准（`docs/architecture.md` 第 10 节）。都要 TCMS_DATABASE_URL，
//! 在里面建一个临时 schema，跑完删掉。

mod index;
mod read;

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
