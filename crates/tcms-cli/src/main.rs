//! 命令行 `tcms`（`docs/spec.md` 6.2）。现在有上下文仓库、索引、读（`get`、`search`）、处理提议、令牌和 MCP 服务端的命令。

mod bench;
mod import;
mod proposals;
mod read;
mod serve;
mod token;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tcms_core::{Kind, Warning, assign_ids, scope_of_path};
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
    /// 编一个上下文包，和 MCP 的 context_get 一样（调试用）。前缀和按任务挑的部分接在一起打印。
    Get {
        /// 岗位。
        #[arg(long)]
        role: String,
        /// 团队。不写时从 teams.yaml 查这个岗位属于哪个团队。
        #[arg(long)]
        team: Option<String>,
        /// 这次任务所属的项目。
        #[arg(long)]
        project: Option<String>,
        /// 这次在做什么。
        #[arg(long)]
        task: String,
        /// 整个包的 token 预算（按字符数估）。
        #[arg(long, default_value_t = 8000)]
        budget: usize,
        /// 今天的日期（YYYY-MM-DD），用来判断有没有过复核日期。默认是今天（UTC）。
        #[arg(long)]
        today: Option<String>,
        /// 用 JSON 输出（前缀、按任务挑的部分、放了哪些条目、cursor）。
        #[arg(long)]
        json: bool,
    },
    /// 全文检索，和 MCP 的 context_search 一样（调试用）。不写 --role 时看全部范围。
    Search {
        /// 要找什么。
        query: String,
        /// 按这个岗位能看的范围找。
        #[arg(long)]
        role: Option<String>,
        /// 团队。不写时从 teams.yaml 查。
        #[arg(long, requires = "role")]
        team: Option<String>,
        /// 项目。
        #[arg(long, requires = "role")]
        project: Option<String>,
        /// 只找这一类：rule、decision、fact、term、procedure、reference、memory。
        #[arg(long)]
        kind: Option<Kind>,
        /// 最多返回几条（不超过 50）。
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// 用 JSON 输出。
        #[arg(long)]
        json: bool,
    },
    /// 列出 agent 的提议（默认只列待审核的）；写了 id 时看这一条：理由、证据、相近的条目和批准以后差在哪里。
    Proposals {
        /// 提议 id，形如 prop_ 加 26 位。
        id: Option<String>,
        /// 列哪些。
        #[arg(long, value_enum, default_value = "open", conflicts_with = "id")]
        status: proposals::Status,
        /// 用 JSON 输出。
        #[arg(long)]
        json: bool,
    },
    /// 批准一条提议：改上下文仓库里的 Markdown，做成一个提交，更新索引。可以先改标题、正文再接受。
    /// 审核的人从环境变量 TCMS_ACTOR（没设时 USER）读，提交者从 TCMS_GIT_NAME、TCMS_GIT_EMAIL 读。
    Accept {
        /// 提议 id。
        id: String,
        /// 改了标题再接受。
        #[arg(long)]
        title: Option<String>,
        /// 改了正文再接受：从这个文件读，写 - 时从标准输入读。
        #[arg(long)]
        body_file: Option<PathBuf>,
        /// 新建、取代时写进哪个文件（仓库里的相对路径，第一层目录要和提议的范围一致）。默认是 <范围>/<类型>.md，取代时放在旧的后面。
        #[arg(long)]
        file: Option<String>,
        /// 条目在提议之后被人改过时，写明看过的是现在的第几版，照样接受（会覆盖成提议的内容，或 --body-file 给的内容）。
        #[arg(long)]
        over: Option<i32>,
        /// 备注，记进审计。
        #[arg(long)]
        note: Option<String>,
        /// 用 JSON 输出。
        #[arg(long)]
        json: bool,
    },
    /// 拒绝一条提议。
    Reject {
        /// 提议 id。
        id: String,
        /// 为什么拒绝，记进审计。
        #[arg(long)]
        note: Option<String>,
    },
    /// 发、列、吊销给 agent 的令牌。
    Token {
        #[command(subcommand)]
        what: token::TokenCmd,
    },
    /// 启动 MCP 服务端（context_get、context_search、context_read、context_changes、context_propose）。
    /// 默认是 Streamable HTTP；--stdio 时走标准输入输出，令牌从 TCMS_TOKEN 读。
    Serve {
        /// 走标准输入输出（给同机、由 agent 启动的进程用）。
        #[arg(long)]
        stdio: bool,
        /// HTTP 监听的地址（--stdio 时不用）。
        #[arg(long, env = "TCMS_LISTEN", default_value = "127.0.0.1:7341")]
        listen: SocketAddr,
        /// HTTP 除了本机以外还接受的 Host（域名、IP，或带端口），可以写多次；环境变量里用逗号隔开。
        #[arg(
            long = "allowed-host",
            env = "TCMS_ALLOWED_HOSTS",
            value_delimiter = ','
        )]
        allowed_hosts: Vec<String>,
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
        Command::Get {
            role,
            team,
            project,
            task,
            budget,
            today,
            json,
        } => {
            let store = connect(&cli.schema).await?;
            let args = read::GetArgs {
                who: read::Who {
                    role,
                    team,
                    project,
                },
                task,
                budget,
                today,
                json,
            };
            read::get(&cli.repo, &store, args).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Search {
            query,
            role,
            team,
            project,
            kind,
            limit,
            json,
        } => {
            let store = connect(&cli.schema).await?;
            let args = read::SearchArgs {
                query,
                who: role.map(|role| read::Who {
                    role,
                    team,
                    project,
                }),
                kind,
                limit,
                json,
            };
            read::search(&cli.repo, &store, args).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Proposals { id, status, json } => {
            let store = connect(&cli.schema).await?;
            match id {
                Some(id) => proposals::show(&store, &id, json).await?,
                None => proposals::list(&store, status, json).await?,
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Accept {
            id,
            title,
            body_file,
            file,
            over,
            note,
            json,
        } => {
            let repo = open_repo(&cli.repo)?;
            let store = connect(&cli.schema).await?;
            proposals::accept(
                &repo,
                &store,
                proposals::AcceptArgs {
                    id,
                    title,
                    body_file,
                    file,
                    over,
                    note,
                    json,
                },
            )
            .await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Reject { id, note } => {
            let store = connect(&cli.schema).await?;
            proposals::reject(&store, &id, note.as_deref()).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Token { what } => {
            let store = connect(&cli.schema).await?;
            token::run(&cli.repo, &store, what).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Serve {
            stdio,
            listen,
            allowed_hosts,
        } => {
            let store = connect(&cli.schema).await?;
            serve::run(
                store,
                serve::ServeArgs {
                    stdio,
                    listen,
                    allowed_hosts,
                },
            )
            .await?;
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

/// 审计里记的“谁”：环境变量 TCMS_ACTOR，没设时用 USER，再没有就是 cli。
fn actor() -> String {
    ["TCMS_ACTOR", "USER"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "cli".to_owned())
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
