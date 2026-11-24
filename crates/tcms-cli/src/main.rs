//! 命令行 `tcms`（`docs/spec.md` 6.2）。现在有上下文仓库、索引、读（`get`、`search`）、处理提议、令牌和 MCP 服务端的命令。

mod affine;
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
use tcms_core::{Kind, Warning};
use tcms_git::{Author, ContextRepo, Remote, RemoteError};
use tcms_store::Store;
use tcms_write::RemoteReport;

/// 内存分配器用 jemalloc，开着后台线程。建索引时用过又放掉的内存（取出的行、tantivy 的写入缓冲、换下来的旧索引），
/// 它过十来秒还给系统；glibc 的 malloc 把这些留在进程里，1 万条换过几次索引以后空闲时约 86 MB，用 jemalloc 约 41 MB
/// （`tcms bench serve`，docs/architecture.md 第 10 节）。
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Parser)]
#[command(name = "tcms", version, about = "面向 agent 的团队上下文系统")]
struct Cli {
    /// 上下文仓库的路径。
    #[arg(long, env = "TCMS_REPO", default_value = ".", global = true)]
    repo: PathBuf,

    /// PostgreSQL 里用的 schema。
    #[arg(long, env = "TCMS_SCHEMA", default_value = "tcms", global = true)]
    schema: String,

    /// 上下文仓库的远端：仓库里配好的远端名（如 origin），或者地址。配了以后，批准、改单条、导入、补 id
    /// 提交之前先拉、提交以后推。https 远端的令牌放在环境变量 TCMS_GIT_TOKEN。
    #[arg(long, env = "TCMS_REMOTE", global = true)]
    remote: Option<String>,

    /// 推到远端的哪个分支。默认和本地当前分支同名。
    #[arg(long, env = "TCMS_REMOTE_BRANCH", global = true)]
    remote_branch: Option<String>,

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
    /// 和远端同步一次：拉（快进或合并）、把索引带到 HEAD、推。要先配远端（--remote 或 TCMS_REMOTE）。
    /// 有错（连不上、合并不了、推送被拒）时退出码不是 0。
    Sync {
        /// 用 JSON 输出。
        #[arg(long)]
        json: bool,
    },
    /// 晋升：把一条岗位、团队、项目的生效条目提到更大的范围。做成一条提议（换了范围的取代），
    /// 要再批准一次才生效。发起的人从环境变量 TCMS_ACTOR（没设时 USER）读。
    Promote {
        /// 条目 id。
        id: String,
        /// 提到哪个范围：team:<团队> 或 org。不写时提到近的那一级（岗位 → 它的团队，团队、项目 → org）。
        #[arg(long)]
        to: Option<String>,
        /// 为什么要提到更大的范围，审核的人会看到。
        #[arg(long)]
        reason: String,
        /// 用 JSON 输出。
        #[arg(long)]
        json: bool,
    },
    /// 系统找的晋升候选：几个岗位（或团队、项目）都写了的相近条目，和最近用得多的岗位、团队、项目条目，
    /// 建议提到哪里。只是列出来，挑一条用 tcms promote 发起。
    SuggestPromotions {
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
    /// 列出待复核的生效条目（复核日期到了的），日期早的在前。
    ReviewDue {
        /// 往后多少天以内到期的也列出来。
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(i32).range(0..=366))]
        within: i32,
        /// 今天的日期（YYYY-MM-DD），默认是今天（UTC）。
        #[arg(long)]
        today: Option<String>,
        /// 用 JSON 输出。
        #[arg(long)]
        json: bool,
    },
    /// 和 AFFiNE 同步：建工作区、接上范围、同步一次、看状态、处理两边都改了的。
    Affine {
        #[command(subcommand)]
        what: affine::AffineCmd,
    },
    /// 发、列、吊销令牌（给 agent 的，和给审核的人的）。
    Token {
        #[command(subcommand)]
        what: token::TokenCmd,
    },
    /// 启动 MCP 服务端（context_get、context_search、context_read、context_changes、context_propose）和审核页。
    /// 默认是 Streamable HTTP（/mcp 和 /review）；--stdio 时只有 MCP，走标准输入输出，令牌从 TCMS_TOKEN 读。
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
        /// 配了远端时，每隔多少秒和远端同步一次（拉人推上去的改动、补推没推上去的提交）。0 是只在批准、改单条时同步。
        /// 环境变量 TCMS_WEBHOOK_SECRET 配了时，远端的推送通知（POST /hooks/push）来了马上同步。
        #[arg(long, env = "TCMS_SYNC_SECS", default_value_t = 60)]
        sync_secs: u64,
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
    // 远端写错了（比如以 - 开头）时，哪个命令都先报出来。
    let remote = remote(&cli)?;
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
            // 拉下来的文件下面补 id 时一起扫，这里不用单独补。
            pull_first(&repo, remote.as_ref(), false).await?;
            if assign_ids_cmd(&repo)? {
                push_after(&repo, remote.as_ref()).await;
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Import { src, into } => {
            let repo = open_repo(&cli.repo)?;
            pull_first(&repo, remote.as_ref(), true).await?;
            import::run(&repo, &src, &into, &author())?;
            push_after(&repo, remote.as_ref()).await;
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
                remote,
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
        Command::Sync { json } => {
            let Some(remote) = remote else {
                bail!("没有配远端：用 --remote 或环境变量 TCMS_REMOTE 写远端名（如 origin）或地址");
            };
            let repo = open_repo(&cli.repo)?;
            let store = connect(&cli.schema).await?;
            let report = tcms_write::sync(&repo, &store, &remote, &author()).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_remote(&report);
            }
            Ok(if report.errors.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Command::Promote {
            id,
            to,
            reason,
            json,
        } => {
            let repo = open_repo(&cli.repo)?;
            let store = connect(&cli.schema).await?;
            proposals::promote(&repo, &store, &id, to.as_deref(), &reason, json).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::SuggestPromotions { json } => {
            let repo = open_repo(&cli.repo)?;
            let store = connect(&cli.schema).await?;
            proposals::suggest_promotions(&repo, &store, json).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Reject { id, note } => {
            let store = connect(&cli.schema).await?;
            proposals::reject(&store, &id, note.as_deref()).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::ReviewDue {
            within,
            today,
            json,
        } => {
            let store = connect(&cli.schema).await?;
            let today = match today {
                Some(d) if tcms_core::valid_date(&d) => d,
                Some(d) => bail!("--today 不是 YYYY-MM-DD：{d}"),
                None => tcms_core::utc_today(),
            };
            let rows = store.items_due(&today, within).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if rows.is_empty() {
                println!(
                    "没有到期要复核的条目（{today}{}）。",
                    if within > 0 {
                        format!(" 往后 {within} 天")
                    } else {
                        String::new()
                    }
                );
            } else {
                for r in &rows {
                    println!(
                        "{}  {}  {} / {}  {}\n  {}  第 {} 版{}",
                        r.review_by.as_deref().unwrap_or("-"),
                        r.id,
                        r.scope,
                        r.kind,
                        r.title,
                        r.path,
                        r.revision,
                        r.owner
                            .as_deref()
                            .map(|o| format!("，负责人 {o}"))
                            .unwrap_or_default()
                    );
                }
                println!(
                    "看过以后改复核日期：在审核页里改这一条，或者直接改 Markdown 里的 review_by。"
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Affine { what } => {
            affine::run(&cli.repo, &cli.schema, remote.as_ref(), what).await
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
            sync_secs,
        } => {
            let store = connect(&cli.schema).await?;
            serve::run(
                store,
                serve::ServeArgs {
                    stdio,
                    listen,
                    allowed_hosts,
                    repo: cli.repo.clone(),
                    remote,
                    sync_every: std::time::Duration::from_secs(sync_secs),
                    webhook_secret: std::env::var("TCMS_WEBHOOK_SECRET")
                        .ok()
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty()),
                },
            )
            .await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Bench { what } => bench::run(what).await,
    }
}

/// 配的远端。`TCMS_GIT_TOKEN` 设了（不是空的）就经凭据助手把它交给 git。
fn remote(cli: &Cli) -> Result<Option<Remote>> {
    let Some(target) = cli.remote.as_deref().filter(|t| !t.trim().is_empty()) else {
        return Ok(None);
    };
    let token = std::env::var_os("TCMS_GIT_TOKEN").is_some_and(|t| !t.is_empty());
    Ok(Some(Remote::new(
        target,
        cli.remote_branch.as_deref(),
        token,
    )?))
}

/// 不经过提交队列的写（导入、补 id）之前先从远端拉。合并不了就停下；连不上只提醒，照样在本地提交。
/// `assign` 为真时，拉下来的条目里没有 id 的补上（和服务同步时一样），提交在后面的写之前。
async fn pull_first(repo: &ContextRepo, remote: Option<&Remote>, assign: bool) -> Result<()> {
    let Some(remote) = remote else {
        return Ok(());
    };
    let dir = repo.workdir().context("上下文仓库没有工作区")?;
    let before = repo.head()?;
    match remote.pull(dir, &author()).await {
        Ok(p) if p.moved_head() => {
            println!("先从远端 {} 拉了：{p}。", remote.display());
            if assign && let Some(done) = tcms_write::assign_since(repo, before, &author())? {
                println!(
                    "拉下来的条目有 {} 条没有 id，补上了，提交 {}：",
                    done.items.len(),
                    done.commit
                );
                for i in &done.items {
                    println!("  {}  {}  {}", i.path, i.id, i.title);
                }
            }
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(e @ RemoteError::NeedsMerge { .. }) => Err(e.into()),
        Err(e) => {
            eprintln!(
                "提醒：从远端 {} 拉取失败（{e}），照样在本地提交",
                remote.display()
            );
            Ok(())
        }
    }
}

/// 提交以后推到远端。推不上去只提醒：提交在本地，`tcms sync` 或服务的定时同步会再推。
async fn push_after(repo: &ContextRepo, remote: Option<&Remote>) {
    let (Some(remote), Some(dir)) = (remote, repo.workdir()) else {
        return;
    };
    match remote.push(dir).await {
        Ok(p) => println!("远端 {}：{p}。", remote.display()),
        Err(e) => eprintln!(
            "提醒：推到远端 {} 失败（{e}）；提交在本地，运行 tcms sync 或等服务定时同步时再推",
            remote.display()
        ),
    }
}

/// 打印和远端同步的结果。
fn print_remote(report: &RemoteReport) {
    println!("{}。", report.summary());
    if report.needs_merge {
        println!("合并好以后再运行 tcms sync。在这之前批准和改单条会停下来。");
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

/// 补 id。做了提交时返回 `true`。
fn assign_ids_cmd(repo: &ContextRepo) -> Result<bool> {
    if repo.head()?.is_none() {
        println!("仓库还没有提交，没有要补的。");
        return Ok(false);
    }
    let Some(done) = tcms_write::assign_all(repo, &author())? else {
        println!("所有条目都有 id 了。");
        return Ok(false);
    };
    println!("补了 {} 条 id，提交 {}：", done.items.len(), done.commit);
    for i in &done.items {
        println!("  {}  {}  {}", i.path, i.id, i.title);
    }
    println!("接着运行 tcms reindex 更新索引。");
    Ok(true)
}
