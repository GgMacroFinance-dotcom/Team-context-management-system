//! `tcms serve`：MCP 服务端和审核页（`docs/architecture.md` 第 7 节）。
//!
//! 条目的变化由 `tcms reindex`（人直接改仓库时）或批准提议写进 PostgreSQL，服务收到通知（或每 30 秒查一次）后换上新的检索索引。
//! 默认是 Streamable HTTP：`/mcp`（每个请求带 agent 的令牌）和审核页 `/review`（审核的人用自己的令牌登录）；
//! `--stdio` 时只有 MCP，走标准输入输出，令牌从 TCMS_TOKEN 读。
//! 两种都只往标准错误写提示，stdio 时标准输出只有协议消息。
//! 配了远端（`TCMS_REMOTE`）时，HTTP 服务每隔 `--sync-secs` 秒和远端同步一次（拉、索引、推），
//! 交给写入线程做，和批准排同一个队；stdio 时不同步。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tcms_git::{Author, Remote};
use tcms_mcp::{HttpOptions, Live};
use tcms_review::{ReviewOptions, Writer};
use tcms_store::Store;

pub struct ServeArgs {
    pub stdio: bool,
    pub listen: SocketAddr,
    pub allowed_hosts: Vec<String>,
    /// 上下文仓库（审核页批准、改单条时写）。
    pub repo: PathBuf,
    /// 上下文仓库的远端。
    pub remote: Option<Remote>,
    /// 多久和远端同步一次，0 是不定时同步。
    pub sync_every: Duration,
}

pub async fn run(store: Store, args: ServeArgs) -> Result<()> {
    // stdio 时先核对令牌，配错了马上知道，不用等第一次调用。
    let secret = if args.stdio {
        let secret = match std::env::var("TCMS_TOKEN") {
            Ok(s) if !s.trim().is_empty() => s.trim().to_owned(),
            _ => bail!("--stdio 要在环境变量 TCMS_TOKEN 里给令牌（tcms token add 发的）"),
        };
        let Some(who) = store.identify(&secret).await.context("核对令牌")? else {
            bail!("TCMS_TOKEN 无效、已吊销或已过期");
        };
        eprintln!(
            "令牌 {}（{}）：岗位 {}{}",
            who.token_id,
            who.name,
            who.role,
            who.team
                .as_deref()
                .map(|t| format!("，团队 {t}"))
                .unwrap_or_default()
        );
        Some(secret)
    } else {
        None
    };

    let live = Live::load(store).await.context("取条目、建检索索引")?;
    report(&live);
    let follow = live.spawn_follow();
    let result = match secret {
        Some(secret) => {
            eprintln!("MCP 服务端（stdio）已启动");
            tcms_mcp::serve_stdio(live, secret)
                .await
                .map_err(anyhow::Error::from)
        }
        None => http(live, args).await,
    };
    follow.abort();
    result
}

async fn http(live: Arc<Live>, args: ServeArgs) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("监听 {}", args.listen))?;
    let addr = listener.local_addr()?;
    if let Err(e) = tcms_git::ContextRepo::open(&args.repo) {
        eprintln!(
            "提醒：打不开上下文仓库 {}（{e}），审核页能看，但批准、改单条会失败；用 --repo 或 TCMS_REPO 指定",
            args.repo.display()
        );
    }
    let writer = Writer::spawn(
        live.store(),
        args.repo.clone(),
        args.remote.clone(),
        args.sync_every,
    )
    .context("起写入线程")?;
    let sync = match &args.remote {
        Some(remote) if !args.sync_every.is_zero() => {
            eprintln!(
                "远端 {}：每 {} 秒同步一次",
                remote.display(),
                args.sync_every.as_secs()
            );
            Some(tokio::spawn(sync_loop(writer.clone(), args.sync_every)))
        }
        Some(remote) => {
            eprintln!("远端 {}：只在批准、改单条时同步", remote.display());
            None
        }
        None => None,
    };
    let app = tcms_mcp::router(
        live.clone(),
        HttpOptions {
            allowed_hosts: args.allowed_hosts.clone(),
        },
    )
    .merge(tcms_review::router(
        live,
        writer,
        ReviewOptions {
            allowed_hosts: args.allowed_hosts.clone(),
            author_email: std::env::var("TCMS_GIT_EMAIL").unwrap_or_default(),
        },
    ));
    eprintln!(
        "MCP 服务端：http://{addr}/mcp；审核页：http://{addr}/review（健康检查 /healthz），Ctrl-C 停止"
    );
    if args.allowed_hosts.is_empty() && !addr.ip().is_loopback() {
        eprintln!(
            "提醒：只接受 Host 是本机的请求；从别的机器用域名或 IP 连时，用 --allowed-host 写上它"
        );
    }
    let served = tcms_mcp::serve_http(listener, app, shutdown())
        .await
        .context("HTTP 服务");
    if let Some(sync) = sync {
        sync.abort();
    }
    served?;
    eprintln!("已停止");
    Ok(())
}

/// 定时和远端同步。启动时马上同步一次（空的上下文仓库这时取到远端的内容）。
/// 只在有变化（拉到了新提交、推上去了）或者出错的情况变了时写日志，同一个错不每次都写。
async fn sync_loop(writer: Writer, every: Duration) {
    let author = Author {
        name: "tcms".to_owned(),
        email: std::env::var("TCMS_GIT_EMAIL")
            .ok()
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| Author::default().email),
    };
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_errors: Vec<String> = Vec::new();
    loop {
        tick.tick().await;
        match writer.sync(author.clone()).await {
            Ok(report) => {
                let errors_changed = report.errors != last_errors;
                if report.head_moved
                    || report.pushed_new
                    || (errors_changed && !report.errors.is_empty())
                {
                    eprintln!("{}", report.summary());
                    if report.needs_merge && errors_changed {
                        eprintln!("合并好之前，批准和改单条会停下来");
                    }
                } else if errors_changed {
                    eprintln!("和远端同步恢复正常");
                }
                last_errors = report.errors;
            }
            Err(e) => {
                let errors = vec![format!("和远端同步失败：{e}")];
                if errors != last_errors {
                    eprintln!("{}", errors[0]);
                }
                last_errors = errors;
            }
        }
    }
}

fn report(live: &Live) {
    let index = live.current();
    let corpus = index.corpus();
    match &corpus.commit {
        Some(c) => eprintln!(
            "取了 {} 条生效条目（提交 {}，变更序号 {}）",
            corpus.len(),
            &c[..c.len().min(12)],
            corpus.cursor
        ),
        None => {
            eprintln!("提醒：索引是空的，先运行 tcms reindex；服务照常启动，索引好以后自动换上")
        }
    }
}

/// Ctrl-C，或者 SIGTERM（systemd、容器停服务时发的）。
async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
