//! `tcms serve`：MCP 服务端和审核页（`docs/architecture.md` 第 7 节）。
//!
//! 条目的变化由 `tcms reindex`（人直接改仓库时）或批准提议写进 PostgreSQL，服务收到通知（或每 30 秒查一次）后换上新的检索索引。
//! 默认是 Streamable HTTP：`/mcp`（每个请求带 agent 的令牌）和审核页 `/review`（审核的人用自己的令牌登录）；
//! `--stdio` 时只有 MCP，走标准输入输出，令牌从 TCMS_TOKEN 读。
//! 两种都只往标准错误写提示，stdio 时标准输出只有协议消息。
//! 配了远端（`TCMS_REMOTE`）时，HTTP 服务每隔 `--sync-secs` 秒和远端同步一次（拉、索引、推），
//! 交给写入线程做，和批准排同一个队；再配了 `TCMS_WEBHOOK_SECRET` 时，远端收到推送通知 `/hooks/push`，马上同步一次。
//! stdio 时不同步。
//! 设了 `TCMS_AFFINE_WORKSPACE`（和连 AFFiNE 的另外几个环境变量，见 `tcms affine`）时，HTTP 服务还一直跟着和 AFFiNE 同步
//! （`docs/affine-sync.md` 7.4）：AFFiNE 里改完停 10 秒、git 里一变就同步，每 5 分钟全部看一遍；连不上 AFFiNE 不影响服务。

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
    /// 远端推送的 webhook 的密钥（环境变量 `TCMS_WEBHOOK_SECRET`）。
    pub webhook_secret: Option<String>,
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

    // 行级安全管不了超级用户和带 BYPASSRLS 的账号（docs/architecture.md 第 5 节）。
    match store.bypasses_row_security().await {
        Ok(false) => {}
        Ok(true) => eprintln!(
            "提醒：连数据库的账号是超级用户或带 BYPASSRLS，数据库的行级安全对它不起作用，只剩服务本身按范围过滤；\
             换成普通账号连（拥有 schema 就够）"
        ),
        Err(e) => eprintln!("提醒：查不了连数据库的账号能不能绕过行级安全：{e}"),
    }
    let live = Live::load(store).await.context("取条目、建检索索引")?;
    report(&live);
    let follow = live.spawn_follow();
    let result = match secret {
        Some(secret) => {
            eprintln!("MCP 服务端（stdio）已启动");
            tcms_mcp::serve_stdio(live.clone(), secret)
                .await
                .map_err(anyhow::Error::from)
        }
        None => http(live.clone(), args).await,
    };
    follow.abort();
    // 停之前把攒着的“条目用了几次”写进库。
    if let Err(e) = live.flush_uses().await {
        eprintln!("记条目用了几次失败：{e}");
    }
    result
}

async fn http(live: Arc<Live>, args: ServeArgs) -> Result<()> {
    let webhook_secret = match (&args.webhook_secret, &args.remote) {
        (Some(s), Some(_)) if s.len() < tcms_review::MIN_SECRET_LEN => bail!(
            "TCMS_WEBHOOK_SECRET 太短，至少 {} 个字符（比如用 openssl rand -hex 32 生成）",
            tcms_review::MIN_SECRET_LEN
        ),
        (Some(s), Some(_)) => Some(s.clone()),
        (Some(_), None) => {
            eprintln!(
                "提醒：配了 TCMS_WEBHOOK_SECRET，但没配远端（TCMS_REMOTE），不接远端推送的通知"
            );
            None
        }
        (None, _) => None,
    };
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
        Some(remote) if !args.sync_every.is_zero() || webhook_secret.is_some() => {
            let mut when = Vec::new();
            if webhook_secret.is_some() {
                when.push("收到推送（POST /hooks/push）马上同步".to_owned());
            }
            if !args.sync_every.is_zero() {
                when.push(format!("每 {} 秒同步一次", args.sync_every.as_secs()));
            }
            eprintln!("远端 {}：{}", remote.display(), when.join("，"));
            Some(writer.spawn_sync_loop(sync_author()))
        }
        Some(remote) => {
            eprintln!("远端 {}：只在批准、改单条时同步", remote.display());
            None
        }
        None => None,
    };
    let affine = crate::affine::serve_env(
        live.store(),
        &args.repo,
        args.remote.as_ref(),
        sync_author(),
    );
    let hook = webhook_secret.map(|secret| tcms_review::push_hook(writer.clone(), secret));
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
    let app = match hook {
        Some(hook) => app.merge(hook),
        None => app,
    };
    eprintln!(
        "MCP 服务端：http://{addr}/mcp；审核页：http://{addr}/review（健康检查 /healthz），Ctrl-C 停止"
    );
    if args.allowed_hosts.is_empty() && !addr.ip().is_loopback() {
        eprintln!(
            "提醒：只接受 Host 是本机的请求；从别的机器用域名或 IP 连时，用 --allowed-host 写上它"
        );
    }
    // 和 AFFiNE 同步在这个任务里跟着跑（不另起线程）；服务停的时候让它做完手上那一次，最多等 20 秒。
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let following = async {
        let Some(env) = affine else { return };
        let mut grace = stopped.clone();
        let run = crate::affine::follow_forever(env, stopped);
        tokio::pin!(run);
        tokio::select! {
            () = &mut run => {}
            () = async {
                let _ = grace.wait_for(|s| *s).await;
                tokio::time::sleep(Duration::from_secs(20)).await;
            } => eprintln!("AFFiNE 同步 20 秒内没停下来，不等了"),
        }
    };
    let serving = async {
        let served = tcms_mcp::serve_http(listener, app, shutdown()).await;
        let _ = stop.send(true);
        served
    };
    let (served, ()) = tokio::join!(serving, following);
    let served = served.context("HTTP 服务");
    if let Some(sync) = sync {
        sync.abort();
    }
    served?;
    eprintln!("已停止");
    Ok(())
}

/// 定时同步、推送后同步做合并提交时的作者。
fn sync_author() -> Author {
    Author {
        name: "tcms".to_owned(),
        email: std::env::var("TCMS_GIT_EMAIL")
            .ok()
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| Author::default().email),
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
pub(crate) async fn shutdown() {
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
