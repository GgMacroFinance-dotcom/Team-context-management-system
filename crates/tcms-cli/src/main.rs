//! 命令行 `tcms`。第 1 步只有和上下文仓库、索引有关的命令（`docs/spec.md` 6.2）。

mod bench;
mod import;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tcms_core::{Warning, assign_ids, scope_of_path};
use tcms_git::{Author, ContextRepo, FileEdit};
use tcms_store::Store;

#[derive(Parser)]
#[command(name = "tcms", version, about = "面向 agent 的团队上下文系统")]
struct Cli {
    /// 上下文仓库的路径。
    #[arg(long, env = "TCMS_REPO", default_value = ".", global = true)]
    repo: PathBuf,

    /// PostgreSQL 里用的 schema。
    #[arg(long, env = "TCMS_SCHEMA", default_value = "tcms", global = true)]
    schema: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 把上下文仓库的 HEAD 同步进 PostgreSQL。数据库地址从环境变量 TCMS_DATABASE_URL 读。
    Reindex {
        /// 解析所有文件，不只是上次以来改过的。版本号保留。
        #[arg(long)]
        full: bool,
        /// 用 JSON 输出结果。
        #[arg(long)]
        json: bool,
    },
    /// 给还没有 id 的条目补上 id，做成一个提交。
    AssignIds,
    /// 把一个 Markdown 目录（比如 Notion、AFFiNE 导出的）导入上下文仓库，做成一个提交。
    Import {
        /// 要导入的目录。
        src: PathBuf,
        /// 放到仓库里的哪个目录，第一层必须是范围，比如 org、team-research、role-macro/资料。
        #[arg(long)]
        into: String,
    },
    /// 性能基准。
    Bench {
        #[command(subcommand)]
        what: bench::What,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("出错了：{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Reindex { full, json } => {
            let repo = open_repo(&cli.repo)?;
            let store = connect(&cli.schema).await?;
            let report = tcms_index::reindex(&repo, &store, full).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if report.up_to_date {
                println!(
                    "索引已经是最新的（{}）。",
                    report.commit.as_deref().unwrap_or("仓库还没有提交")
                );
            } else {
                println!(
                    "索引到 {}：{}解析了 {} 个文件、{} 条，变更 {} 条，用时 {} 毫秒。",
                    report.commit.as_deref().unwrap_or("-"),
                    if report.full { "全量，" } else { "" },
                    report.files_parsed,
                    report.items_parsed,
                    report.changed(),
                    report.took.as_millis()
                );
                for (kind, n) in &report.counts {
                    println!("  {kind}: {n}");
                }
                print_warnings(&report.warnings);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::AssignIds => {
            let repo = open_repo(&cli.repo)?;
            assign_ids_cmd(&repo)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Import { src, into } => {
            let repo = open_repo(&cli.repo)?;
            import::run(&repo, &src, &into, &author())?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Bench { what } => bench::run(what).await,
    }
}

fn open_repo(path: &std::path::Path) -> Result<ContextRepo> {
    ContextRepo::open(path).with_context(|| format!("打开上下文仓库 {}", path.display()))
}

async fn connect(schema: &str) -> Result<Store> {
    let url = database_url()?;
    Store::connect(&url, schema)
        .await
        .context("连接 PostgreSQL")
}

fn database_url() -> Result<String> {
    match std::env::var("TCMS_DATABASE_URL") {
        Ok(url) if !url.is_empty() => Ok(url),
        _ => bail!("没有设环境变量 TCMS_DATABASE_URL（PostgreSQL 连接串）"),
    }
}

/// 提交者：环境变量 TCMS_GIT_NAME、TCMS_GIT_EMAIL，没设时是 tcms <tcms@localhost>。
fn author() -> Author {
    let mut a = Author::default();
    if let Ok(name) = std::env::var("TCMS_GIT_NAME")
        && !name.is_empty()
    {
        a.name = name;
    }
    if let Ok(email) = std::env::var("TCMS_GIT_EMAIL")
        && !email.is_empty()
    {
        a.email = email;
    }
    a
}

fn print_warnings(warnings: &[Warning]) {
    if warnings.is_empty() {
        return;
    }
    println!("提醒 {} 条：", warnings.len());
    for w in warnings {
        match w.line {
            Some(l) => println!("  {}:{l}  {}", w.path, w.message),
            None => println!("  {}  {}", w.path, w.message),
        }
    }
}

fn assign_ids_cmd(repo: &ContextRepo) -> Result<()> {
    let Some(head) = repo.head()? else {
        println!("仓库还没有提交，没有要补的。");
        return Ok(());
    };
    let mut edits = Vec::new();
    let mut lines = Vec::new();
    for (path, blob) in repo.files(head)? {
        if scope_of_path(&path).is_none() {
            continue;
        }
        let Ok(text) = String::from_utf8(repo.read_blob(blob)?) else {
            continue;
        };
        if let Some((new, ids)) = assign_ids(&path, &text) {
            for (id, title) in &ids {
                lines.push(format!("{path}  {id}  {title}"));
            }
            edits.push(FileEdit {
                path,
                content: Some(new.into_bytes()),
            });
        }
    }
    if edits.is_empty() {
        println!("所有条目都有 id 了。");
        return Ok(());
    }
    let message = format!("补 id：{} 条\n\n{}\n", lines.len(), lines.join("\n"));
    let commit = repo.commit(&edits, &message, &author())?;
    println!("补了 {} 条 id，提交 {commit}：", lines.len());
    for l in &lines {
        println!("  {l}");
    }
    println!("接着运行 tcms reindex 更新索引。");
    Ok(())
}
