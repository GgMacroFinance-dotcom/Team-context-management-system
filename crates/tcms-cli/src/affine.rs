//! `tcms affine`：和 AFFiNE 同步（`docs/affine-sync.md`）。现在是一次一次跑（A3）；`tcms serve` 里一直跟着同步在 A3 的第二部分。
//!
//! 连 AFFiNE 的设置都在环境变量里：`TCMS_AFFINE_URL`（比如 `http://127.0.0.1:3010`）、`TCMS_AFFINE_EMAIL`、
//! `TCMS_AFFINE_PASSWORD`（给本系统建的专门账号），`TCMS_AFFINE_WORKSPACE`（工作区 id，`tcms affine init` 建），
//! 可选 `TCMS_AFFINE_CA`（自托管用自己的 CA 时的 PEM 文件）。

use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::{Subcommand, ValueEnum};
use tcms_affine::{Context, Keep, Server, SyncReport};
use tcms_core::Scope;
use tcms_git::Remote;
use tcms_store::Store;

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
            (tcms_affine::sync(&cx, &mut server, &resume).await?, json)
        }
        AffineCmd::Resolve { id, keep, json } => {
            let keep = match keep {
                Side::Git => Keep::Git,
                Side::Affine => Keep::Affine,
            };
            let path = tcms_affine::resolve(&store, id, keep).await?;
            if !json {
                println!("第 {id} 条（{path}）处理了，接着同步一次。");
            }
            (tcms_affine::sync(&cx, &mut server, &[]).await?, json)
        }
        AffineCmd::Init { .. } | AffineCmd::Status { .. } => unreachable!("上面处理过"),
    };
    print_report(&report, json)?;
    Ok(if report.needs_attention() {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
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
        let state = match d.state {
            "linked" => "接上了",
            "file_created" => "AFFiNE 里新建的，建了文件",
            "synced" => "同步了",
            "paused" => "停着",
            "unlinked" => "git 里删了，文档移进了 AFFiNE 的回收站",
            "skipped" => "没接",
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
        println!(
            "{}  {state}{}",
            d.path,
            if parts.is_empty() {
                String::new()
            } else {
                format!("（{}）", parts.join("；"))
            }
        );
        if let Some(p) = &d.paused {
            println!("  {p}");
        }
        for n in &d.notes {
            println!("  {n}");
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
    if report.needs_attention() {
        println!("有停着的文档或等人处理的条目，用 tcms affine status 看。");
    }
    Ok(())
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
