//! `tcms get` 和 `tcms search`：和 MCP 的 `context_get`、`context_search` 一样的读法，调试用。

use std::path::Path;

use anyhow::{Context, Result, bail};
use tcms_core::{Kind, TEAMS_FILE, Teams, utc_today, valid_date};
use tcms_git::{ContextRepo, ObjectId};
use tcms_pack::{Request, compile};
use tcms_search::{Corpus, SearchIndex, SearchOptions, Visibility};
use tcms_store::Store;

pub struct Who {
    pub role: String,
    pub team: Option<String>,
    pub project: Option<String>,
}

pub struct GetArgs {
    pub who: Who,
    pub task: String,
    pub budget: usize,
    pub today: Option<String>,
    pub json: bool,
}

pub async fn get(repo: &Path, store: &Store, args: GetArgs) -> Result<()> {
    let today = match args.today {
        Some(d) if valid_date(&d) => d,
        Some(d) => bail!("--today 不是一个日期（应为 YYYY-MM-DD）：{d}"),
        None => utc_today(),
    };
    let index = load(store).await?;
    let (visibility, mut notes) = resolve(repo, index.corpus(), &args.who)?;
    let pack = compile(
        &index,
        &Request {
            visibility: &visibility,
            task: &args.task,
            budget_tokens: args.budget,
            today: &today,
        },
    )?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&pack)?);
    } else {
        print!("{}", pack.text());
        notes.extend(pack.warnings.iter().cloned());
        eprintln!(
            "\n—— 前缀约 {} token，按任务挑的部分约 {} token，预算 {}；全文 {} 条，目录 {} 条，没列出 {} 条；cursor {}",
            pack.prefix_tokens,
            pack.task_tokens,
            pack.budget_tokens,
            pack.included.len(),
            pack.listed.len(),
            pack.omitted,
            pack.cursor
        );
    }
    for n in notes {
        eprintln!("提醒：{n}");
    }
    Ok(())
}

pub struct SearchArgs {
    pub query: String,
    pub who: Option<Who>,
    pub kind: Option<Kind>,
    pub limit: usize,
    pub json: bool,
}

pub async fn search(repo: &Path, store: &Store, args: SearchArgs) -> Result<()> {
    let index = load(store).await?;
    let (visibility, notes) = match &args.who {
        Some(who) => resolve(repo, index.corpus(), who)?,
        None => (Visibility::All, Vec::new()),
    };
    let hits = index.search(
        &args.query,
        &visibility,
        &SearchOptions {
            kind: args.kind,
            limit: args.limit,
        },
    )?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else if hits.is_empty() {
        println!("没有命中。");
    } else {
        for h in &hits {
            println!(
                "{} r{} · {} · {} · 分数 {:.3}\n  {}\n  {}",
                h.id,
                h.revision,
                h.kind,
                h.scope,
                h.score as f64 / 1000.0,
                h.title,
                h.excerpt
            );
        }
    }
    for n in notes {
        eprintln!("提醒：{n}");
    }
    Ok(())
}

async fn load(store: &Store) -> Result<SearchIndex> {
    let corpus = Corpus::load(store).await.context("从 PostgreSQL 取条目")?;
    if corpus.commit.is_none() {
        bail!("索引是空的，先运行 tcms reindex");
    }
    Ok(SearchIndex::build(corpus)?)
}

/// 岗位、团队、项目 → 能看的范围。没写团队时从索引到的那个提交里的 `teams.yaml` 查。
fn resolve(repo: &Path, corpus: &Corpus, who: &Who) -> Result<(Visibility, Vec<String>)> {
    let mut notes = Vec::new();
    let team = match &who.team {
        Some(t) => Some(t.clone()),
        None => {
            let teams = teams_at(repo, corpus.commit.as_deref())?;
            match teams.as_ref().and_then(|t| t.team_of_role(&who.role)) {
                Some(t) => Some(t.to_owned()),
                None => {
                    notes.push(format!(
                        "{TEAMS_FILE} 里没有岗位 {}，这次只看 org、岗位和项目；可以用 --team 指定团队",
                        who.role
                    ));
                    None
                }
            }
        }
    };
    let v = Visibility::agent(&who.role, team.as_deref(), who.project.as_deref())?;
    Ok((v, notes))
}

pub(crate) fn teams_at(repo: &Path, commit: Option<&str>) -> Result<Option<Teams>> {
    let Some(commit) = commit else {
        return Ok(None);
    };
    let repo = ContextRepo::open(repo).with_context(|| {
        format!(
            "打开上下文仓库 {}（查 {TEAMS_FILE}；也可以用 --team 指定团队）",
            repo.display()
        )
    })?;
    let id = ObjectId::from_hex(commit.as_bytes())
        .with_context(|| format!("索引里的提交号不对：{commit}"))?;
    let Some(blob) = repo.files(id)?.get(TEAMS_FILE).copied() else {
        return Ok(None);
    };
    let text = String::from_utf8(repo.read_blob(blob)?).context("teams.yaml 不是 UTF-8")?;
    Ok(Some(Teams::parse(&text).map_err(anyhow::Error::msg)?))
}
