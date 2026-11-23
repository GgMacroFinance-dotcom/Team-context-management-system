//! `tcms token`：发、列、吊销给 agent 的令牌（`docs/spec.md` 6.1）。库里只存令牌的哈希，明文只在发的时候打印一次。

use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use tcms_core::{Scope, TEAMS_FILE, valid_name};
use tcms_git::ContextRepo;
use tcms_store::{ALL_PROJECTS, NewToken, Store, TokenRow};

use crate::actor;
use crate::read::teams_at;

#[derive(Subcommand)]
pub enum TokenCmd {
    /// 发一个令牌。令牌决定调用者的岗位、团队和能声明的项目；明文只打印这一次。
    Add {
        /// 给谁用的，比如“I.P. 宏观研报”。
        #[arg(long)]
        name: String,
        /// 岗位。
        #[arg(long)]
        role: String,
        /// 团队。不写时从上下文仓库 HEAD 的 teams.yaml 查这个岗位属于哪个团队。
        #[arg(long)]
        team: Option<String>,
        /// 允许在 context_get 里声明的项目，可以写多次。
        #[arg(long = "project")]
        projects: Vec<String>,
        /// 允许声明任何项目。
        #[arg(long, conflicts_with = "projects")]
        all_projects: bool,
        /// 多少天后过期（1 到 3650）。不写时不过期。
        #[arg(long, value_parser = clap::value_parser!(i32).range(1..=3650))]
        days: Option<i32>,
        /// 用 JSON 输出（含明文）。
        #[arg(long)]
        json: bool,
    },
    /// 列出全部令牌（不含明文）。
    List {
        /// 用 JSON 输出。
        #[arg(long)]
        json: bool,
    },
    /// 吊销一个令牌，马上生效（HTTP 下一个请求、stdio 下一次调用就不认了）。
    Revoke {
        /// 令牌 id，形如 tok_ 加 26 位。
        id: String,
    },
}

pub async fn run(repo: &Path, store: &Store, cmd: TokenCmd) -> Result<()> {
    match cmd {
        TokenCmd::Add {
            name,
            role,
            team,
            projects,
            all_projects,
            days,
            json,
        } => {
            let new = resolve(
                repo,
                NewToken {
                    name,
                    role,
                    team,
                    projects: if all_projects {
                        vec![ALL_PROJECTS.to_owned()]
                    } else {
                        projects
                    },
                    expires_in_days: days,
                },
            )?;
            let (id, secret) = store.create_token(&new, &actor()).await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "id": id,
                        "secret": secret,
                        "name": new.name,
                        "role": new.role,
                        "team": new.team,
                        "projects": new.projects,
                        "expires_in_days": new.expires_in_days,
                    }))?
                );
            } else {
                println!(
                    "发了令牌 {id}：{}",
                    describe(&new.role, new.team.as_deref(), &new.projects)
                );
                println!("令牌明文只显示这一次，请马上放进 agent 的环境变量或密钥设置：");
                println!("{secret}");
            }
        }
        TokenCmd::List { json } => {
            let rows = store.list_tokens().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if rows.is_empty() {
                println!("还没有令牌。用 tcms token add 发一个。");
            } else {
                for r in &rows {
                    println!(
                        "{}  {}  {}\n  {}；发于 {}{}",
                        r.id,
                        status(r),
                        r.name,
                        describe(&r.role, r.team.as_deref(), &r.projects),
                        r.created_at,
                        match &r.expires_at {
                            Some(e) => format!("，{e} 过期"),
                            None => String::new(),
                        }
                    );
                }
            }
        }
        TokenCmd::Revoke { id } => {
            if store.revoke_token(&id, &actor()).await? {
                println!("已吊销 {id}。");
            } else {
                bail!("没有这个令牌，或者早已吊销：{id}");
            }
        }
    }
    Ok(())
}

/// 检查名字；没写团队时从 HEAD 的 teams.yaml 查。
fn resolve(repo: &Path, mut t: NewToken) -> Result<NewToken> {
    if t.name.trim().is_empty() {
        bail!("--name 不能是空的");
    }
    if !valid_name(&t.role) {
        bail!("岗位名不对：{}", t.role);
    }
    if let Some(team) = &t.team
        && !valid_name(team)
    {
        bail!("团队名不对：{team}");
    }
    if let Some(p) = t
        .projects
        .iter()
        .find(|p| p.as_str() != ALL_PROJECTS && !valid_name(p))
    {
        bail!("项目名不对：{p}");
    }
    t.projects.sort();
    t.projects.dedup();

    let teams = match ContextRepo::open(repo) {
        Ok(r) => match r.head()? {
            Some(head) => teams_at(repo, Some(&head.to_string()))?,
            None => None,
        },
        Err(e) if t.team.is_none() => {
            return Err(e).with_context(|| {
                format!(
                    "打开上下文仓库 {} 查 {TEAMS_FILE}（也可以用 --team 指定团队）",
                    repo.display()
                )
            });
        }
        Err(_) => None,
    };
    if let Some(teams) = &teams {
        match (&t.team, teams.team_of_role(&t.role)) {
            (None, Some(found)) => t.team = Some(found.to_owned()),
            (None, None) => eprintln!(
                "提醒：{TEAMS_FILE} 里没有岗位 {}，这个令牌没有团队，只能看 org、岗位和项目；可以用 --team 指定",
                t.role
            ),
            (Some(given), Some(found)) if given != found => eprintln!(
                "提醒：{TEAMS_FILE} 里岗位 {} 属于团队 {found}，这里写的是 {given}",
                t.role
            ),
            _ => {}
        }
        for p in t.projects.iter().filter(|p| p.as_str() != ALL_PROJECTS) {
            if !teams.defines(&Scope::Project(p.clone())) {
                eprintln!("提醒：{TEAMS_FILE} 里没有定义项目 {p}");
            }
        }
    }
    Ok(t)
}

fn describe(role: &str, team: Option<&str>, projects: &[String]) -> String {
    let projects = if projects.iter().any(|p| p == ALL_PROJECTS) {
        "任何项目".to_owned()
    } else if projects.is_empty() {
        "不能声明项目".to_owned()
    } else {
        format!("项目 {}", projects.join("、"))
    };
    format!(
        "岗位 {role}，{}，{projects}",
        match team {
            Some(t) => format!("团队 {t}"),
            None => "没有团队".to_owned(),
        }
    )
}

fn status(r: &TokenRow) -> &'static str {
    if r.revoked_at.is_some() {
        "已吊销"
    } else if r.expired {
        "已过期"
    } else {
        "有效"
    }
}
