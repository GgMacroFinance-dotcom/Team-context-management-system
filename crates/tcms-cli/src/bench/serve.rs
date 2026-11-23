//! `tcms bench serve`：用 `bench read` 一样的数据，起一个真的 `tcms serve --stdio` 子进程（就是正在跑的这个程序），
//! 用 MCP 客户端连上去量：
//!
//! - 启动：取条目、建检索索引、握手；
//! - `context_get`、`context_search` 经过 stdio 的一次往返（含每次调用核对令牌）；
//! - 令牌核对本身（查一次 PostgreSQL）；
//! - 加一个文件、`tcms reindex` 以后，服务多久能搜到新内容（收到通知后整份重建）；
//! - 子进程处理过请求以后的物理内存，换过 1 次和 5 次索引以后的（看重建会不会让内存越涨越多）。

use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, JsonObject};
use rmcp::service::{RoleClient, RunningService};
use serde_json::{Value, json};
use tcms_core::ItemId;
use tcms_git::{Author, ContextRepo, FileEdit};
use tcms_store::{ALL_PROJECTS, NewToken, Store};
use tokio::io::AsyncReadExt;

use super::data::{Gen, make_repo};
use super::{Percentiles, ms, percentiles, rss_kb, temp_schema};

/// `context_get` 经过 stdio 一次往返的 p95 门槛（1 万条、8 千 token）。编包本身的门槛是 20 毫秒。
const GET_P95_LIMIT: Duration = Duration::from_millis(30);
/// `context_search` 经过 stdio 一次往返的 p95 门槛。检索本身的门槛是 10 毫秒。
const SEARCH_P95_LIMIT: Duration = Duration::from_millis(15);
/// 核对一次令牌的 p95 门槛。
const IDENTIFY_P95_LIMIT: Duration = Duration::from_millis(5);
/// 加一个文件、索引完，到服务里搜得到的门槛（1 万条）。
const SWAP_LIMIT: Duration = Duration::from_secs(2);
/// 换几次索引，看物理内存稳不稳。
const SWAPS: usize = 5;
/// 换过 [`SWAPS`] 次索引以后子进程物理内存的门槛（MB，1 万条）。修之前 5 次约 220 MB，还在涨。
const RSS_LIMIT_MB: u64 = 160;
/// 新内容里的词，别的条目里没有。
const MARKER: &str = "benchswapmarker";

pub async fn run(items: usize, requests: usize, budget: usize, check: bool) -> Result<ExitCode> {
    ensure!(items > 0 && requests > 0, "条数和次数都要大于 0");
    let url = crate::database_url()?;
    let (_dir, repo, mut gen_, files) = make_repo(items)?;
    let schema = temp_schema();
    let store = Store::connect(&url, &schema)
        .await
        .context("连接 PostgreSQL")?;
    let result = measure(&repo, &store, &url, &schema, &mut gen_, requests, budget).await;
    store.drop_schema().await?;
    let m = result?;

    println!(
        "服务的基准：{items} 条、{files} 个文件，tcms serve --stdio 子进程，context_get（预算 {budget} token）、context_search、令牌核对各 {requests} 次"
    );
    println!(
        "  启动（取条目、建索引、握手）        {:>8.1} 毫秒",
        ms(m.startup)
    );
    let row = |name: &str, p: &Percentiles, limit: Duration| {
        println!(
            "  {name}中位数 {:>6.2} 毫秒   p95 {:>6.2} 毫秒   门槛 p95 {:.0} 毫秒",
            ms(p.p50),
            ms(p.p95),
            ms(limit)
        );
    };
    row("context_get 往返  ", &m.get, GET_P95_LIMIT);
    row("context_search 往返", &m.search, SEARCH_P95_LIMIT);
    row("核对令牌          ", &m.identify, IDENTIFY_P95_LIMIT);
    println!(
        "  加一个文件到服务里搜得到            {:>8.1} 毫秒（其中 tcms reindex {:.1} 毫秒）   门槛 {:.0} 毫秒",
        ms(m.swap),
        ms(m.reindex),
        ms(SWAP_LIMIT)
    );
    println!(
        "  子进程物理内存：处理过请求后 {}，换过 1 次索引后 {}，换过 {SWAPS} 次后 {}（门槛 {RSS_LIMIT_MB} MB）",
        mb(m.rss_served),
        mb(m.rss_swapped),
        mb(m.rss_swapped_all)
    );

    let mut failed = false;
    for (bad, what) in [
        (m.get.p95 > GET_P95_LIMIT, "context_get 往返"),
        (m.search.p95 > SEARCH_P95_LIMIT, "context_search 往返"),
        (m.identify.p95 > IDENTIFY_P95_LIMIT, "核对令牌"),
        (m.swap > SWAP_LIMIT, "换上新索引"),
        (
            m.rss_swapped_all.is_some_and(|kb| kb > RSS_LIMIT_MB * 1024),
            "换过几次索引后的物理内存",
        ),
    ] {
        if bad {
            eprintln!("{what}超过门槛");
            failed = true;
        }
    }
    Ok(if check && failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn mb(kb: Option<u64>) -> String {
    match kb {
        Some(kb) => format!("约 {:.0} MB", kb as f64 / 1024.0),
        None => "未知".to_owned(),
    }
}

struct Measured {
    startup: Duration,
    get: Percentiles,
    search: Percentiles,
    identify: Percentiles,
    reindex: Duration,
    swap: Duration,
    rss_served: Option<u64>,
    rss_swapped: Option<u64>,
    rss_swapped_all: Option<u64>,
}

async fn call(client: &RunningService<RoleClient, ()>, name: &str, args: Value) -> Result<Value> {
    let Value::Object(args) = args else {
        bail!("参数要是对象");
    };
    let args: JsonObject = args;
    let r = client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(args))
        .await
        .with_context(|| format!("调用 {name}"))?;
    if r.is_error == Some(true) {
        bail!("{name} 出错：{:?}", r.content);
    }
    r.structured_content
        .with_context(|| format!("{name} 没有结构化结果"))
}

async fn measure(
    repo: &ContextRepo,
    store: &Store,
    url: &str,
    schema: &str,
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
    let token = NewToken {
        name: "基准".to_owned(),
        role: "r1".to_owned(),
        team: Some("t1".to_owned()),
        projects: vec![ALL_PROJECTS.to_owned()],
        expires_in_days: None,
    };
    let (_, secret) = store.create_token(&token, "bench").await?;

    let mut identify = Vec::with_capacity(requests);
    for _ in 0..requests {
        let t = Instant::now();
        let who = store.identify(&secret).await?;
        identify.push(t.elapsed());
        ensure!(who.is_some(), "刚发的令牌核对不上");
    }

    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args(["serve", "--stdio"])
        .env("TCMS_DATABASE_URL", url)
        .env("TCMS_SCHEMA", schema)
        .env("TCMS_TOKEN", &secret)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("起 tcms serve --stdio")?;
    let pid = child.id();
    let (Some(stdout), Some(stdin), Some(mut stderr)) =
        (child.stdout.take(), child.stdin.take(), child.stderr.take())
    else {
        bail!("拿不到子进程的标准输入输出");
    };
    let log = tokio::spawn(async move {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s).await;
        s
    });
    let t = Instant::now();
    let client = match ().serve((stdout, stdin)).await {
        Ok(c) => c,
        Err(e) => {
            let _ = child.kill().await;
            bail!(
                "连不上 tcms serve --stdio：{e}\n{}",
                log.await.unwrap_or_default()
            );
        }
    };
    let startup = t.elapsed();

    let tasks: Vec<(String, Option<String>)> = (0..requests)
        .map(|i| {
            let n = gen_.rng.range(6, 30);
            (
                gen_.text(n),
                i.is_multiple_of(2).then(|| format!("p{}", i % 3)),
            )
        })
        .collect();
    let queries: Vec<String> = (0..requests)
        .map(|_| {
            let n = gen_.rng.range(1, 3);
            (0..n).map(|_| gen_.word()).collect::<Vec<_>>().join(" ")
        })
        .collect();
    let get_args = |(task, project): &(String, Option<String>)| {
        let mut a = json!({"role": "r1", "team": "t1", "task": task, "budget_tokens": budget});
        if let Some(p) = project {
            a["project"] = json!(p);
        }
        a
    };

    // 先热一下，再量。
    call(&client, "context_get", get_args(&tasks[0])).await?;
    call(&client, "context_search", json!({"query": queries[0]})).await?;
    let mut get = Vec::with_capacity(requests);
    for task in &tasks {
        let t = Instant::now();
        let v = call(&client, "context_get", get_args(task)).await?;
        get.push(t.elapsed());
        ensure!(v["included"].is_array(), "context_get 的结果不对");
    }
    let mut search = Vec::with_capacity(requests);
    for q in &queries {
        let t = Instant::now();
        let v = call(&client, "context_search", json!({"query": q})).await?;
        search.push(t.elapsed());
        ensure!(v["hits"].is_array(), "context_search 的结果不对");
    }
    let rss_served = pid.and_then(|p| rss_kb(Some(p)));

    // 加一个文件，量到服务里搜得到要多久；再加几次，看物理内存稳不稳。
    let mut swaps = Vec::new();
    let mut reindex = Duration::ZERO;
    let mut rss_swapped = Vec::new();
    for round in 1..=SWAPS {
        let marker = format!("{MARKER}{round}");
        repo.commit(
            &[FileEdit {
                path: format!("org/换索引-{round}.md"),
                content: Some(
                    format!(
                        "## 换索引 {round}\n<!-- ctx id={} kind=fact -->\n{marker} 刚加的。\n",
                        ItemId::generate()
                    )
                    .into_bytes(),
                ),
            }],
            "加一个文件",
            &Author::default(),
        )?;
        let t = Instant::now();
        tcms_index::reindex(repo, store, false).await?;
        if round == 1 {
            reindex = t.elapsed();
        }
        loop {
            let v = call(&client, "context_search", json!({"query": marker})).await?;
            if v["hits"].as_array().is_some_and(|h| !h.is_empty()) {
                break;
            }
            ensure!(
                t.elapsed() < Duration::from_secs(60),
                "60 秒内服务没换上新索引"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        swaps.push(t.elapsed());
        for task in tasks.iter().take(50) {
            call(&client, "context_get", get_args(task)).await?;
        }
        // 等旧的那份放掉。
        tokio::time::sleep(Duration::from_millis(300)).await;
        rss_swapped.push(pid.and_then(|p| rss_kb(Some(p))));
    }

    client.cancel().await.context("断开 MCP")?;
    if tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
        bail!("标准输入关了以后 tcms serve --stdio 5 秒内没退出");
    }
    Ok(Measured {
        startup,
        get: percentiles(get),
        search: percentiles(search),
        identify: percentiles(identify),
        reindex,
        swap: swaps[0],
        rss_served,
        rss_swapped: rss_swapped[0],
        rss_swapped_all: rss_swapped[SWAPS - 1],
    })
}
