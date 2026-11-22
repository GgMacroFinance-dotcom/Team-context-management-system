//! `tcms bench index`：在临时目录里造一个上下文仓库，用 TCMS_DATABASE_URL 指的库里一个临时 schema，
//! 量全量索引、没有变更时的全量、只改一个文件后的增量。跑完删掉临时 schema。

use std::fmt::Write as _;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tcms_core::{ItemId, Kind};
use tcms_git::{Author, ContextRepo, FileEdit};
use tcms_store::Store;

use super::{ITEMS_PER_FILE, ms, scope_dir, temp_schema};

/// 1 万条全量索引的门槛。
const FULL_LIMIT_10K: Duration = Duration::from_secs(3);
/// 改一个文件后增量索引的门槛。
const INCREMENTAL_LIMIT: Duration = Duration::from_millis(200);

fn file_text(file: usize, ids: &[ItemId], version: usize) -> String {
    let mut s = format!(
        "---\nkind: {}\ntags: [基准, f{file}]\n---\n\n# 文件 {file}\n",
        Kind::ALL[file % 6]
    );
    for (i, id) in ids.iter().enumerate() {
        let _ = write!(
            s,
            "\n## 条目 {file}-{i}\n<!-- ctx id={id} -->\n\n这是第 {version} 版。团队在周报里用表格，日期写全，数字注明来源和口径；\
             遇到口径变化要在脚注里说明，并且和上一期对比。Lorem ipsum dolor sit amet, consectetur adipiscing elit.\n"
        );
    }
    s
}

pub async fn run(items: usize, check: bool) -> Result<ExitCode> {
    let url = crate::database_url()?;
    let files = items.div_ceil(ITEMS_PER_FILE).max(1);
    let dir = tempfile::tempdir()?;
    let repo = ContextRepo::init(dir.path())?;
    let author = Author::default();

    let mut ids: Vec<Vec<ItemId>> = Vec::with_capacity(files);
    let mut edits = Vec::with_capacity(files);
    for f in 0..files {
        let n = ITEMS_PER_FILE.min(items - f * ITEMS_PER_FILE);
        let file_ids: Vec<ItemId> = (0..n).map(|_| ItemId::generate()).collect();
        edits.push(FileEdit {
            path: format!("{}/f{f:05}.md", scope_dir(f)),
            content: Some(file_text(f, &file_ids, 1).into_bytes()),
        });
        ids.push(file_ids);
    }
    repo.commit(&edits, "基准数据", &author)?;

    let store = Store::connect(&url, &temp_schema())
        .await
        .context("连接 PostgreSQL")?;
    let result = measure(&repo, &store, &ids, &author).await;
    store.drop_schema().await?;
    let m = result?;

    let full_limit = FULL_LIMIT_10K.mul_f64(items as f64 / 10_000.0);
    println!("索引基准：{items} 条、{files} 个文件");
    println!("  只读 git 和解析     {:>8.1} 毫秒", ms(m.collect));
    println!(
        "  全量索引（空库）     {:>8.1} 毫秒   门槛 {:.0} 毫秒",
        ms(m.full),
        ms(full_limit)
    );
    println!("  全量索引（没有变更） {:>8.1} 毫秒", ms(m.full_noop));
    println!(
        "  改一个文件后增量     {:>8.1} 毫秒   门槛 {:.0} 毫秒",
        ms(m.incremental),
        ms(INCREMENTAL_LIMIT)
    );

    let mut failed = false;
    if m.full > full_limit {
        eprintln!("全量索引超过门槛");
        failed = true;
    }
    if m.incremental > INCREMENTAL_LIMIT {
        eprintln!("增量索引超过门槛");
        failed = true;
    }
    Ok(if check && failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

struct Measured {
    collect: Duration,
    full: Duration,
    full_noop: Duration,
    incremental: Duration,
}

async fn measure(
    repo: &ContextRepo,
    store: &Store,
    ids: &[Vec<ItemId>],
    author: &Author,
) -> Result<Measured> {
    let t = Instant::now();
    let collected = tcms_index::collect(repo, None)?;
    let collect = t.elapsed();
    anyhow::ensure!(
        collected.items.len() == ids.iter().map(Vec::len).sum::<usize>(),
        "解析出的条数不对"
    );

    let r = tcms_index::reindex(repo, store, false).await?;
    let full = r.took;
    anyhow::ensure!(r.counts.get("created").copied().unwrap_or(0) == collected.items.len());

    let r = tcms_index::reindex(repo, store, true).await?;
    let full_noop = r.took;
    anyhow::ensure!(r.changed() == 0, "没有变更的全量索引不该有变更");

    let target = ids.len() / 2;
    repo.commit(
        &[FileEdit {
            path: format!("{}/f{target:05}.md", scope_dir(target)),
            content: Some(file_text(target, &ids[target], 2).into_bytes()),
        }],
        "改一个文件",
        author,
    )?;
    let r = tcms_index::reindex(repo, store, false).await?;
    let incremental = r.took;
    anyhow::ensure!(r.changed() == ids[target].len(), "增量索引的变更条数不对");

    Ok(Measured {
        collect,
        full,
        full_noop,
        incremental,
    })
}
