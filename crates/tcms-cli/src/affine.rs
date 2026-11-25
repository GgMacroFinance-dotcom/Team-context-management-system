//! `tcms affine`：和 AFFiNE 同步（`docs/affine-sync.md`）。可以一次一次跑（`sync`），也可以一直跟着同步
//! （`follow`；`tcms serve` 设了 `TCMS_AFFINE_WORKSPACE` 时自己会跟着同步，[`follow_forever`]）。
//!
//! 连 AFFiNE 的设置都在环境变量里：`TCMS_AFFINE_URL`（比如 `http://127.0.0.1:3010`）、`TCMS_AFFINE_EMAIL`、
//! `TCMS_AFFINE_PASSWORD`（给本系统建的专门账号），`TCMS_AFFINE_WORKSPACE`（工作区 id，`tcms affine init` 建），
//! 可选 `TCMS_AFFINE_CA`（自托管用自己的 CA 时的 PEM 文件）。

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::{Subcommand, ValueEnum};
use tcms_affine::{
    CommentReport, Context, DocReport, FollowEnv, FollowEvent, FollowLink, FollowOptions, Keep,
    Server, SyncReport, sync_comments,
};
use tcms_core::Scope;
use tcms_git::{Author, Remote};
use tcms_store::Store;
use tokio::sync::watch;

#[derive(Subcommand)]
pub enum AffineCmd {
    /// 在 AFFiNE 里新建一个专门放团队上下文的工作区，打印它的 id（填进 TCMS_AFFINE_WORKSPACE）。
    Init {
        /// 工作区的名字。
        #[arg(long, default_value = "团队上下文")]
        name: String,
    },
    /// 把一个范围（或范围里的一个文件）接到 AFFiNE：工作区根上建一个文件夹，每个文件建一篇文档，每条一个二级标题。
    /// 整个范围接上以后，范围里新加的文件、AFFiNE 里这个文件夹下新建的文档，同步时也会接上。
    Link {
        /// 范围：org、team:x、role:x、project:x。
        scope: String,
        /// 只接这一个文件（仓库里的相对路径）。
        #[arg(long)]
        file: Option<String>,
        /// 用 JSON 输出。
        #[arg(long)]
        json: bool,
    },
    /// 同步一次：两边谁改了就写到另一边；两边都改了的记下来等人处理（tcms affine status 看）。
    /// 有停着的文档或等人处理的条目时退出码是 2。
    Sync {
        /// 这个文件停过（一次要归档一半以上的条目、AFFiNE 里移进了回收站……），看过了，这次接着做。可以写多次。
        #[arg(long)]
        resume: Vec<String>,
        /// 用 JSON 输出。
        #[arg(long)]
        json: bool,
    },
    /// 一直跟着同步（前台跑，Ctrl-C 停）：AFFiNE 里改完停 10 秒、git 里一变就同步，每 5 分钟全部看一遍。
    /// 两边都没变的不拿提交队列的锁。`tcms serve` 设了 TCMS_AFFINE_WORKSPACE 时自己会跟着同步，不用另外跑这个。
    Follow,
    /// 接上了哪些范围、文件，哪些停着，哪些条目等人处理。不连 AFFiNE。
    Status {
        /// 用 JSON 输出。
        #[arg(long)]
        json: bool,
    },
    /// 处理一条等人处理的（编号见 tcms affine status），然后同步一次，把留下的那边写到另一边。
    Resolve {
        /// 编号。
        id: i64,
        /// 留哪一边。
        #[arg(long, value_enum)]
        keep: Side,
        /// 用 JSON 输出。
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Side {
    /// 留上下文仓库里的。
    Git,
    /// 留 AFFiNE 里的。
    Affine,
}

fn env(name: &str) -> Result<String> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Ok(v.trim().to_owned()),
        _ => bail!("没有设环境变量 {name}"),
    }
}

async fn server() -> Result<Server> {
    let config = affine_sync_client::Config {
        url: env("TCMS_AFFINE_URL")?,
        credentials: affine_sync_client::Credentials {
            email: env("TCMS_AFFINE_EMAIL")?,
            password: env("TCMS_AFFINE_PASSWORD")?,
        },
        client_version: None,
        extra_ca: std::env::var_os("TCMS_AFFINE_CA")
            .filter(|v| !v.is_empty())
            .map(Into::into),
    };
    Server::connect(config).await.context("连 AFFiNE")
}

pub async fn run(
    repo_path: &std::path::Path,
    schema: &str,
    remote: Option<&Remote>,
    cmd: AffineCmd,
) -> Result<ExitCode> {
    if let AffineCmd::Init { name } = &cmd {
        let mut server = server().await?;
        let ws = server.create_workspace(name).await.context("新建工作区")?;
        println!("建好了工作区「{name}」：{ws}");
        println!("把它填进环境变量 TCMS_AFFINE_WORKSPACE，再用 tcms affine link <范围> 接上范围。");
        return Ok(ExitCode::SUCCESS);
    }
    let store = crate::connect(schema).await?;
    if let AffineCmd::Status { json } = cmd {
        return status(&store, json).await;
    }
    let repo = crate::open_repo(repo_path)?;
    let workspace = env("TCMS_AFFINE_WORKSPACE")?;
    let actor = crate::actor();
    let author = crate::author();
    if let AffineCmd::Follow = cmd {
        let env = FollowEnv {
            repo: repo_path.to_owned(),
            store,
            workspace,
            actor,
            remote: remote.cloned(),
            author,
            link: FollowLink::default(),
            public_url: public_url(),
        };
        let (stop, stopped) = watch::channel(false);
        let run = follow_forever(env, stopped);
        tokio::pin!(run);
        tokio::select! {
            () = &mut run => {}
            () = crate::serve::shutdown() => {
                eprintln!("停止中：做完手上这一次同步就停");
                let _ = stop.send(true);
                run.await;
            }
        }
        return Ok(ExitCode::SUCCESS);
    }
    let cx = Context {
        repo: &repo,
        store: &store,
        workspace: &workspace,
        actor: &actor,
        remote,
        author: &author,
    };
    let mut server = server().await?;
    let (report, json) = match cmd {
        AffineCmd::Link { scope, file, json } => {
            let scope: Scope = scope.parse().map_err(anyhow::Error::msg)?;
            (
                tcms_affine::link(&cx, &mut server, &scope, file.as_deref()).await?,
                json,
            )
        }
        AffineCmd::Sync { resume, json } => {
            let mut report = tcms_affine::sync(&cx, &mut server, &resume).await?;
            report.comments = Some(comments(&cx, &mut server).await?);
            (report, json)
        }
        AffineCmd::Resolve { id, keep, json } => {
            let keep = match keep {
                Side::Git => Keep::Git,
                Side::Affine => Keep::Affine,
            };
            let path = tcms_affine::resolve(&store, id, keep, cx.actor).await?;
            if !json {
                println!("第 {id} 条（{path}）处理了，接着同步一次。");
            }
            let mut report = tcms_affine::sync(&cx, &mut server, &[]).await?;
            report.comments = Some(comments(&cx, &mut server).await?);
            (report, json)
        }
        AffineCmd::Init { .. } | AffineCmd::Status { .. } | AffineCmd::Follow => {
            unreachable!("上面处理过")
        }
    };
    print_report(&report, json)?;
    Ok(if report.needs_attention() {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
}

/// 同步完看一遍提议显示成的评论（`docs/affine-sync.md` 7.6）。
async fn comments(cx: &Context<'_>, server: &mut Server) -> Result<CommentReport> {
    let url = public_url();
    Ok(sync_comments(cx.store, server, cx.workspace, url.as_deref()).await?)
}

/// 评论这次做了什么，一样一行；什么都没做是空的。
fn comment_lines(r: &CommentReport) -> Vec<String> {
    let mut lines = Vec::new();
    let mut add = |what: &str, ids: &[String]| {
        if !ids.is_empty() {
            lines.push(format!(
                "评论：{what} {} 条（{}）",
                ids.len(),
                ids.join("、")
            ));
        }
    };
    add("新发了", &r.posted);
    add("有新回复", &r.replies);
    add("回复了结果、标成已解决", &r.closed);
    add("在 AFFiNE 里被删了", &r.gone);
    lines.extend(
        r.errors
            .iter()
            .map(|e| format!("评论出错（下次再试）：{e}")),
    );
    lines
}

/// 一篇文档这次怎么样了：一行，停着的原因、备注各缩进一行。
fn doc_lines(d: &DocReport) -> Vec<String> {
    let state = match d.state {
        "linked" => "接上了",
        "file_created" => "AFFiNE 里新建的，建了文件",
        "synced" => "同步了",
        "paused" => "停着",
        "unlinked" => "git 里删了，文档移进了 AFFiNE 的回收站",
        "skipped" => "没接",
        "unchanged" if d.held > 0 => "别的条目两边都没变",
        "unchanged" => "两边都没变",
        other => other,
    };
    let mut parts = Vec::new();
    let counts =
        |c: &tcms_affine::Counts| format!("改 {}、加 {}、删 {}", c.updated, c.added, c.removed);
    if d.to_git.total() > 0 {
        parts.push(format!("写进 git：{}", counts(&d.to_git)));
    }
    if d.to_affine.total() > 0 {
        parts.push(format!("写进 AFFiNE：{}", counts(&d.to_affine)));
    }
    if d.held > 0 {
        parts.push(format!("{} 条等人处理", d.held));
    }
    let mut lines = vec![format!(
        "{}  {state}{}",
        d.path,
        if parts.is_empty() {
            String::new()
        } else {
            format!("（{}）", parts.join("；"))
        }
    )];
    lines.extend(d.paused.iter().map(|p| format!("  {p}")));
    lines.extend(d.notes.iter().map(|n| format!("  {n}")));
    lines
}

fn print_report(report: &SyncReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    let mut quiet = 0;
    for d in &report.docs {
        if d.state == "unchanged" {
            quiet += 1;
            continue;
        }
        for line in doc_lines(d) {
            println!("{line}");
        }
    }
    if quiet > 0 {
        println!("另外 {quiet} 篇两边都没变。");
    }
    if report.docs.is_empty() {
        println!("没有接上的文件。");
    }
    if !report.commits.is_empty() {
        println!(
            "提交了 {} 个：{}",
            report.commits.len(),
            report.commits.join("、")
        );
    }
    if let Some(r) = &report.remote {
        crate::print_remote(r);
    }
    for line in report.comments.iter().flat_map(comment_lines) {
        println!("{line}");
    }
    if report.needs_attention() {
        println!("有停着的文档或等人处理的条目，用 tcms affine status 看。");
    }
    Ok(())
}

/// `tcms serve` 里跟着同步的设置：设了 `TCMS_AFFINE_WORKSPACE` 才跟着同步，没设是 `None`。
pub fn serve_env(
    store: &Store,
    repo: &Path,
    remote: Option<&Remote>,
    author: Author,
) -> Option<FollowEnv> {
    let workspace = std::env::var("TCMS_AFFINE_WORKSPACE")
        .ok()
        .map(|w| w.trim().to_owned())
        .filter(|w| !w.is_empty())?;
    Some(FollowEnv {
        repo: repo.to_owned(),
        store: store.clone(),
        workspace,
        actor: crate::actor(),
        remote: remote.cloned(),
        author,
        link: FollowLink::default(),
        public_url: public_url(),
    })
}

/// 审核页对外的地址（`TCMS_PUBLIC_URL`，比如 `https://ctx.example.com`），写进 AFFiNE 评论里的链接。
fn public_url() -> Option<String> {
    std::env::var("TCMS_PUBLIC_URL")
        .ok()
        .map(|u| u.trim().trim_end_matches('/').to_owned())
        .filter(|u| !u.is_empty())
}

/// 连上 AFFiNE（连不上隔一会儿再试，5 秒起每次翻倍，最多 5 分钟），然后一直跟着同步，直到 `stop` 变成 `true`。
/// 往标准错误写有变化的文档和出的错。
pub async fn follow_forever(env: FollowEnv, mut stop: watch::Receiver<bool>) {
    let mut wait = Duration::from_secs(5);
    let mut said: Option<String> = None;
    env.link.connecting(&env.workspace, None);
    let mut server = loop {
        match server().await {
            Ok(s) => break s,
            Err(e) => {
                let msg = format!("{e:#}");
                env.link.connecting(&env.workspace, Some(msg.clone()));
                if said.as_deref() != Some(msg.as_str()) {
                    eprintln!("AFFiNE 同步：连不上（{msg}），过一会儿再试");
                    said = Some(msg);
                }
            }
        }
        tokio::select! {
            _ = stop.wait_for(|s| *s) => return,
            () = tokio::time::sleep(wait) => {}
        }
        wait = (wait * 2).min(Duration::from_secs(300));
    };
    eprintln!(
        "AFFiNE 同步：连上了，工作区 {}，一直跟着同步（AFFiNE 里改完停 10 秒、git 里一变就同步，每 5 分钟全部看一遍）",
        env.workspace
    );
    let mut log = FollowLog::default();
    tcms_affine::follow(&env, &mut server, FollowOptions::default(), stop, |ev| {
        log.on(ev)
    })
    .await;
}

/// 跟着同步时往标准错误写的：只写有变化的文档；同一篇没接、停着，同一个错，只写一次。
#[derive(Default)]
struct FollowLog {
    /// 文件 → 上次写的“没接”“停着”。
    said: HashMap<String, String>,
    error: Option<String>,
    /// 上次写的评论出的错。
    comment_errors: String,
}

impl FollowLog {
    fn on(&mut self, ev: FollowEvent<'_>) {
        match ev {
            FollowEvent::Synced(_, report) => {
                if self.error.take().is_some() {
                    eprintln!("AFFiNE 同步：又好了");
                }
                // 只记下了等人处理的、别的都没变的文档也写一行（下次同步跳过它们，所以只写这一次）。
                for d in report
                    .docs
                    .iter()
                    .filter(|d| d.state != "unchanged" || d.held > 0)
                {
                    let lines = doc_lines(d);
                    if matches!(d.state, "skipped" | "paused") {
                        let text = lines.join("\n");
                        if self.said.get(&d.path) == Some(&text) {
                            continue;
                        }
                        self.said.insert(d.path.clone(), text);
                    } else {
                        self.said.remove(&d.path);
                    }
                    for line in lines {
                        eprintln!("AFFiNE 同步：{line}");
                    }
                }
                if !report.commits.is_empty() {
                    eprintln!("AFFiNE 同步：提交了 {}", report.commits.join("、"));
                    if let Some(r) = &report.remote {
                        eprintln!("AFFiNE 同步：{}", r.summary());
                    }
                }
            }
            FollowEvent::Comments(r) => {
                // 同一个错一直在的话（比如一篇文档取不到评论）只写一次。
                let errors = r.errors.join("\n");
                for line in comment_lines(r) {
                    if line.starts_with("评论出错") && self.comment_errors == errors {
                        continue;
                    }
                    eprintln!("AFFiNE 同步：{line}");
                }
                self.comment_errors = errors;
            }
            FollowEvent::Failed(e) => {
                let msg = e.to_string();
                if self.error.as_deref() != Some(msg.as_str()) {
                    eprintln!("AFFiNE 同步出错，过一会儿再试：{msg}");
                    self.error = Some(msg);
                }
            }
            FollowEvent::Disconnected(reason) => {
                eprintln!("AFFiNE 同步：连接断了（{reason}），在重连")
            }
            FollowEvent::Reconnected => eprintln!("AFFiNE 同步：又连上了，全部看一遍"),
        }
    }
}

async fn status(store: &Store, json: bool) -> Result<ExitCode> {
    let s = tcms_affine::status(store).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(ExitCode::SUCCESS);
    }
    if s.scopes.is_empty() {
        println!("还没有接上 AFFiNE（tcms affine link <范围>）。");
        return Ok(ExitCode::SUCCESS);
    }
    for sc in &s.scopes {
        println!(
            "范围 {}：工作区 {}，{}",
            sc.scope,
            sc.workspace_id,
            if sc.whole {
                "整个范围"
            } else {
                "只接了几个文件"
            }
        );
    }
    println!("文件 {} 个：", s.docs.len());
    for d in &s.docs {
        println!(
            "  {}  {} 条  上次同步 {}{}",
            d.path,
            d.items,
            d.synced_at,
            d.paused
                .as_deref()
                .map(|p| format!("\n    停着：{p}"))
                .unwrap_or_default()
        );
    }
    if s.conflicts.is_empty() {
        println!("没有等人处理的。");
    } else {
        println!(
            "等人处理的 {} 条（tcms affine resolve <编号> --keep git|affine）：",
            s.conflicts.len()
        );
        for c in &s.conflicts {
            let reason = tcms_affine::plan::Reason::parse(&c.reason)
                .map_or(c.reason.as_str(), |r| r.label());
            let title = c
                .git_title
                .as_deref()
                .or(c.affine_title.as_deref())
                .or(c.base_title.as_deref())
                .unwrap_or("");
            println!(
                "  第 {} 条  {}  「{title}」  {reason}{}",
                c.id,
                c.path,
                c.message
                    .as_deref()
                    .map(|m| format!("：{m}"))
                    .unwrap_or_default()
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}
