//! `tcms serve`：MCP 服务端（`docs/architecture.md` 第 7 节）。
//!
//! 服务只读：条目的变化由 `tcms reindex` 写进 PostgreSQL，服务收到通知（或每 30 秒查一次）后换上新的检索索引。
//! 默认是 Streamable HTTP（`/mcp`，每个请求带 Bearer 令牌）；`--stdio` 时走标准输入输出，令牌从 TCMS_TOKEN 读。
//! 两种都只往标准错误写提示，stdio 时标准输出只有协议消息。

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tcms_mcp::{HttpOptions, Live};
use tcms_store::Store;

pub struct ServeArgs {
    pub stdio: bool,
    pub listen: SocketAddr,
    pub allowed_hosts: Vec<String>,
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
    eprintln!("MCP 服务端：http://{addr}/mcp（健康检查 /healthz），Ctrl-C 停止");
    if args.allowed_hosts.is_empty() && !addr.ip().is_loopback() {
        eprintln!(
            "提醒：只接受 Host 是本机的请求；从别的机器用域名或 IP 连时，用 --allowed-host 写上它"
        );
    }
    tcms_mcp::serve_http(
        live,
        listener,
        HttpOptions {
            allowed_hosts: args.allowed_hosts,
        },
        shutdown(),
    )
    .await
    .context("HTTP 服务")?;
    eprintln!("已停止");
    Ok(())
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
