//! `tcms bench read`：造一个内容有变化的上下文仓库（中文词、英文词、数字，常见词出现得多），
//! 索引进临时 schema，然后量：从 PostgreSQL 取出全部生效条目、建检索索引、编上下文包、检索。
//! 编包和检索各跑若干次，报中位数和 p95。数据见 [`super::data`]。

use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use tcms_git::ContextRepo;
use tcms_pack::{Request, compile};
use tcms_search::{Corpus, SearchIndex, SearchOptions, Visibility};
use tcms_store::Store;

use super::data::{Gen, make_repo};
use super::{Percentiles, ms, percentiles, rss_kb, temp_schema};

/// 编一个上下文包的 p95 门槛（1 万条、8 千 token）。
const PACK_P95_LIMIT: Duration = Duration::from_millis(20);
/// 检索的 p95 门槛（1 万条）。
const SEARCH_P95_LIMIT: Duration = Duration::from_millis(10);
/// 判断复核日期用的“今天”，固定下来，每次结果一样。
const TODAY: &str = "2026-11-23";

pub async fn run(items: usize, requests: usize, budget: usize, check: bool) -> Result<ExitCode> {
    ensure!(items > 0 && requests > 0, "条数和次数都要大于 0");
    let url = crate::database_url()?;
    let (_dir, repo, mut gen_, files) = make_repo(items)?;

    let store = Store::connect(&url, &temp_schema())
        .await
        .context("连接 PostgreSQL")?;
    let result = measure(&repo, &store, &mut gen_, requests, budget).await;
    store.drop_schema().await?;
    let m = result?;

    println!(
        "读的基准：{items} 条、{files} 个文件，编包 {requests} 次（预算 {budget} token），检索 {requests} 次"
    );
    println!("  从 PostgreSQL 取出生效条目 {:>8.1} 毫秒", ms(m.load));
    println!("  建检索索引               {:>8.1} 毫秒", ms(m.build));
    println!(
        "  编上下文包     中位数 {:>6.2} 毫秒   p95 {:>6.2} 毫秒   门槛 p95 {:.0} 毫秒",
        ms(m.pack.p50),
        ms(m.pack.p95),
        ms(PACK_P95_LIMIT)
    );
    println!(
        "  检索           中位数 {:>6.2} 毫秒   p95 {:>6.2} 毫秒   门槛 p95 {:.0} 毫秒",
        ms(m.search.p50),
        ms(m.search.p95),
        ms(SEARCH_P95_LIMIT)
    );
    println!(
        "  取条目和建索引多用的内存 {}（进程物理内存前后差）",
        m.memory
    );
    println!(
        "  平均每个包：可见 {:.0} 条，全文 {:.1} 条，目录 {:.1} 条，约 {:.0} token",
        m.visible, m.included, m.listed, m.tokens
    );

    let mut failed = false;
    if m.pack.p95 > PACK_P95_LIMIT {
        eprintln!("编上下文包超过门槛");
        failed = true;
    }
    if m.search.p95 > SEARCH_P95_LIMIT {
        eprintln!("检索超过门槛");
        failed = true;
    }
    Ok(if check && failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

struct Measured {
    load: Duration,
    build: Duration,
    pack: Percentiles,
    search: Percentiles,
    visible: f64,
    included: f64,
    listed: f64,
    tokens: f64,
    /// 取条目、建索引前后进程物理内存的差。
    memory: String,
}

fn identity(i: usize) -> Result<Visibility> {
    let role = format!("r{}", i % 10);
    let team = format!("t{}", i % 5);
    let project = i.is_multiple_of(2).then(|| format!("p{}", i % 3));
    Ok(Visibility::agent(&role, Some(&team), project.as_deref())?)
}

async fn measure(
    repo: &ContextRepo,
    store: &Store,
    gen_: &mut Gen,
    requests: usize,
    budget: usize,
) -> Result<Measured> {
    let r = tcms_index::reindex(repo, store, false).await?;
    ensure!(
        r.warnings.is_empty(),
        "基准数据有提醒：{:?}",
        &r.warnings[..1]
    );

    let rss_before = rss_kb(None);
    let t = Instant::now();
    let corpus = Corpus::load(store).await?;
    let load = t.elapsed();
    let t = Instant::now();
    let index = SearchIndex::build(corpus)?;
    let build = t.elapsed();
    let memory = match (rss_before, rss_kb(None)) {
        (Some(a), Some(b)) => format!("约 {:.0} MB", b.saturating_sub(a) as f64 / 1024.0),
        _ => "未知".to_owned(),
    };

    let tasks: Vec<String> = (0..requests)
        .map(|_| {
            let n = gen_.rng.range(6, 30);
            gen_.text(n)
        })
        .collect();
    let queries: Vec<String> = (0..requests)
        .map(|_| {
            let n = gen_.rng.range(1, 3);
            (0..n).map(|_| gen_.word()).collect::<Vec<_>>().join(" ")
        })
        .collect();
    let ids: Vec<Visibility> = (0..requests).map(identity).collect::<Result<_>>()?;

    // 先热一下，再量。
    let req = |i: usize| Request {
        visibility: &ids[i],
        task: &tasks[i],
        budget_tokens: budget,
        today: TODAY,
    };
    let first = compile(&index, &req(0))?;
    ensure!(
        first == compile(&index, &req(0))?,
        "同一个请求两次编出来不一样"
    );

    let (mut pack_times, mut search_times) = (Vec::new(), Vec::new());
    let (mut included, mut listed, mut tokens, mut visible) = (0usize, 0usize, 0usize, 0usize);
    for (i, visibility) in ids.iter().enumerate() {
        let t = Instant::now();
        let p = compile(&index, &req(i))?;
        pack_times.push(t.elapsed());
        ensure!(
            p.prefix_tokens > budget || p.prefix_tokens + p.task_tokens <= budget,
            "包超出了预算"
        );
        included += p.included.len();
        listed += p.listed.len();
        tokens += p.prefix_tokens + p.task_tokens;
        visible += index
            .corpus()
            .items()
            .iter()
            .filter(|it| visibility.allows(&it.scope))
            .count();
    }
    for (query, visibility) in queries.iter().zip(&ids) {
        let t = Instant::now();
        let hits = index.search(query, visibility, &SearchOptions::default())?;
        search_times.push(t.elapsed());
        std::hint::black_box(hits);
    }
    let n = requests as f64;
    Ok(Measured {
        load,
        build,
        pack: percentiles(pack_times),
        search: percentiles(search_times),
        visible: visible as f64 / n,
        included: included as f64 / n,
        listed: listed as f64 / n,
        tokens: tokens as f64 / n,
        memory,
    })
}
