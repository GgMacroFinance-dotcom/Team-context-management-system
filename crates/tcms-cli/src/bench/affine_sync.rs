//! `tcms bench affine`：和 AFFiNE 同步的基准（`docs/affine-sync.md` 的 A5）。上下文仓库在临时目录里，
//! PostgreSQL 用 TCMS_DATABASE_URL 指的库里一个临时 schema，AFFiNE 换成这个进程里的假 AFFiNE（内存里的
//! Yjs 文档，收发更新、存评论），量的是本系统这一侧：
//!
//! - 第一次接上：一个范围 1 千条（50 个文件），在 AFFiNE 里建文件夹和 50 篇文档、记下底稿；
//! - 全部看一遍、两边都没变（跟着同步时每 5 分钟一次）；
//! - 跟着同步（`follow`，等人停手的 10 秒改成 0）：AFFiNE 里改一条，从收到广播到 git 里提交完；
//!   git 里改一条，从提交、更新索引到写进 AFFiNE；
//! - 跟着同步、空闲时比开始跟着同步之前多用的物理内存；
//! - 提议显示成评论：给 100 条提议发评论，再看一遍（都发过了，只取回复）。
//!
//! 连真的 AFFiNE 时还要加上网络往返（同步协议取、推一篇各一次），这里量不到。

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

use affine_sync_client::{Comment, Error as ClientError};
use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};
use tcms_affine::workspace::set_workspace_name;
use tcms_affine::{
    Affine, Context, Event, FollowEnv, FollowEvent, FollowLink, FollowOptions, Job, RandomIds,
    Section, follow, link, split, sync, sync_comments, to_blocks,
};
use tcms_core::{ItemId, Scope};
use tcms_git::{Author, ContextRepo, FileEdit};
use tcms_store::{Action, NewProposal, NewToken, Proposed, Store};
use tokio::sync::mpsc;
use yrs::updates::decoder::Decode;
use yrs::{Doc, ReadTxn, StateVector, Transact, Update};

use super::{ITEMS_PER_FILE, Percentiles, ms, percentiles, rss_kb, temp_schema};

/// 第一次接上 1 千条的门槛（条数不同时按比例换算）。
const LINK_LIMIT_1K: Duration = Duration::from_secs(2);
/// 全部看一遍、两边都没变（1 千条），p95 的门槛（条数不同时按比例换算）。
const FULL_LIMIT_1K: Duration = Duration::from_millis(200);
/// AFFiNE 里改一条到 git 里提交完，p95 的门槛。不随条数换算：只同步改了的那一篇（`docs/affine-sync.md` 7.8）。
const TO_GIT_LIMIT: Duration = Duration::from_millis(250);
/// git 里改一条到写进 AFFiNE，p95 的门槛。不随条数换算。
const TO_AFFINE_LIMIT: Duration = Duration::from_millis(250);
/// 看一遍评论（100 条都发过），p95 的门槛。不随条数换算。
const COMMENTS_LIMIT: Duration = Duration::from_millis(100);
/// 跟着同步、空闲时多用的物理内存的门槛。
const MEMORY_LIMIT_MB: i64 = 20;

const WS: &str = "bench-workspace";
const SCOPE: &str = "role:bench";
const FULL_RUNS: usize = 10;
const PROPOSALS: usize = 100;
const COMMENT_RUNS: usize = 10;
/// 等一次跟着同步做完，最多等多久。
const WAIT: Duration = Duration::from_secs(30);

/// 内存里的假 AFFiNE。
#[derive(Default)]
struct Fake {
    docs: HashMap<String, Doc>,
    comments: HashMap<String, Vec<Comment>>,
    count: usize,
}

impl Fake {
    fn doc(&self, id: &str) -> Result<&Doc> {
        self.docs
            .get(id)
            .with_context(|| format!("假 AFFiNE 里没有 {id}"))
    }

    fn comment_mut(&mut self, id: &str) -> Result<&mut Comment, ClientError> {
        self.comments
            .values_mut()
            .flatten()
            .find(|c| c.id == id)
            .ok_or_else(|| ClientError::Protocol(format!("没有评论 {id}")))
    }

    fn new_comment(&mut self, content: Value) -> Comment {
        self.count += 1;
        Comment {
            id: format!("comment-{}", self.count),
            content,
            resolved: false,
            user_id: "bench".to_owned(),
            user_name: "基准".to_owned(),
            created_at: String::new(),
            updated_at: String::new(),
            replies: Vec::new(),
        }
    }
}

/// 同步程序和测量的一方共用一个假 AFFiNE；测量的一方模拟人改了以后发一个广播。
struct Shared {
    fake: Rc<RefCell<Fake>>,
    events: mpsc::UnboundedReceiver<Event>,
}

impl Affine for Shared {
    async fn load(
        &mut self,
        _ws: &str,
        doc: &str,
        sv: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, ClientError> {
        let fake = self.fake.borrow();
        let Some(d) = fake.docs.get(doc) else {
            return Ok(None);
        };
        let sv = match sv {
            Some(b) => {
                StateVector::decode_v1(b).map_err(|e| ClientError::Protocol(e.to_string()))?
            }
            None => StateVector::default(),
        };
        Ok(Some(d.transact().encode_state_as_update_v1(&sv)))
    }

    async fn push(
        &mut self,
        _ws: &str,
        doc: &str,
        update: &[u8],
        _create: bool,
    ) -> Result<(), ClientError> {
        let update = Update::decode_v1(update).map_err(|e| ClientError::Protocol(e.to_string()))?;
        let mut fake = self.fake.borrow_mut();
        let d = fake.docs.entry(doc.to_owned()).or_default();
        d.transact_mut()
            .apply_update(update)
            .map_err(|e| ClientError::Protocol(e.to_string()))
    }

    async fn editors(&mut self, _ws: &str) -> Result<HashMap<String, String>, ClientError> {
        Ok(self
            .fake
            .borrow()
            .docs
            .keys()
            .map(|k| (k.clone(), "基准".to_owned()))
            .collect())
    }

    async fn comments(&mut self, _ws: &str, doc: &str) -> Result<Vec<Comment>, ClientError> {
        Ok(self
            .fake
            .borrow()
            .comments
            .get(doc)
            .cloned()
            .unwrap_or_default())
    }

    async fn create_comment(
        &mut self,
        _ws: &str,
        doc: &str,
        _title: &str,
        content: Value,
    ) -> Result<String, ClientError> {
        let mut fake = self.fake.borrow_mut();
        let c = fake.new_comment(content);
        let id = c.id.clone();
        fake.comments.entry(doc.to_owned()).or_default().push(c);
        Ok(id)
    }

    async fn reply(&mut self, id: &str, _title: &str, content: Value) -> Result<(), ClientError> {
        let mut fake = self.fake.borrow_mut();
        let r = fake.new_comment(content);
        fake.comment_mut(id)?.replies.push(r);
        Ok(())
    }

    async fn resolve_comment(&mut self, id: &str) -> Result<(), ClientError> {
        self.fake.borrow_mut().comment_mut(id)?.resolved = true;
        Ok(())
    }

    async fn next_event(&mut self) -> Event {
        match self.events.recv().await {
            Some(e) => e,
            None => std::future::pending().await,
        }
    }
}

fn path(file: usize) -> String {
    format!("role-bench/f{file:05}.md")
}

fn item_body(file: usize, item: usize, version: usize) -> String {
    format!(
        "第 {file}-{item} 条，第 {version} 版。团队在周报里用表格，日期写全，**数字注明来源和口径**；\
         遇到口径变化要在脚注里说明，并且和上一期对比。\n\n- 写明数据的发布日期\n- 和上一期对比"
    )
}

fn file_text(file: usize, ids: &[ItemId], versions: &[usize]) -> String {
    let mut s = format!("---\nkind: fact\n---\n\n# 文件 {file}\n");
    for (i, id) in ids.iter().enumerate() {
        let _ = write!(
            s,
            "\n## 条目 {file}-{i}\n<!-- ctx id={id} -->\n\n{}\n",
            item_body(file, i, versions[i])
        );
    }
    s
}

pub async fn run(items: usize, edits: usize, check: bool) -> Result<ExitCode> {
    let url = crate::database_url()?;
    let files = items.div_ceil(ITEMS_PER_FILE).max(1);
    let dir = tempfile::tempdir()?;
    let repo = ContextRepo::init(dir.path())?;
    let author = Author::default();
    let mut ids: Vec<Vec<ItemId>> = Vec::with_capacity(files);
    let mut edits_in = Vec::with_capacity(files);
    for f in 0..files {
        let n = ITEMS_PER_FILE.min(items - f * ITEMS_PER_FILE);
        let file_ids: Vec<ItemId> = (0..n).map(|_| ItemId::generate()).collect();
        edits_in.push(FileEdit {
            path: path(f),
            content: Some(file_text(f, &file_ids, &vec![1; n]).into_bytes()),
        });
        ids.push(file_ids);
    }
    repo.commit(&edits_in, "基准数据", &author)?;

    let store = Store::connect(&url, &temp_schema())
        .await
        .context("连接 PostgreSQL")?;
    let result = measure(dir.path(), &repo, &store, &ids, &author, edits).await;
    store.drop_schema().await?;
    let m = result?;

    let link_limit = LINK_LIMIT_1K.mul_f64(items as f64 / 1000.0);
    let full_limit = FULL_LIMIT_1K.mul_f64(items as f64 / 1000.0);
    let line = |name: &str, p: &Percentiles, limit: Duration| {
        println!(
            "  {name}  中位数 {:>7.1} 毫秒  p95 {:>7.1} 毫秒   门槛 p95 {:.0} 毫秒",
            ms(p.p50),
            ms(p.p95),
            ms(limit)
        );
    };
    println!("和 AFFiNE 同步的基准：{items} 条、{files} 篇文档（假 AFFiNE，在这个进程里）");
    println!(
        "  第一次接上                     {:>7.1} 毫秒   门槛 {:.0} 毫秒",
        ms(m.link),
        ms(link_limit)
    );
    line("全部看一遍、两边都没变    ", &m.full, full_limit);
    line("AFFiNE 里改一条到提交完   ", &m.to_git, TO_GIT_LIMIT);
    line("git 里改一条到写进 AFFiNE ", &m.to_affine, TO_AFFINE_LIMIT);
    println!(
        "  给 {PROPOSALS} 条提议发评论             {:>7.1} 毫秒",
        ms(m.post_comments)
    );
    line("看一遍评论（都发过）      ", &m.comments, COMMENTS_LIMIT);
    let mb = |kb: Option<u64>| {
        kb.map_or("量不到".to_owned(), |k| {
            format!("{:.1} MB", k as f64 / 1024.0)
        })
    };
    let grew = m.grew_mb();
    println!(
        "  物理内存：跟着同步之前 {}，跟着同步、空闲 {:.0} 秒以后 {}，变化 {}（多用的门槛 {MEMORY_LIMIT_MB} MB）",
        mb(m.rss_before),
        m.idle_wait.as_secs_f64(),
        mb(m.rss_idle),
        grew.map_or("量不到".to_owned(), |g| format!("{g:+.1} MB")),
    );

    let mut failed = Vec::new();
    if m.link > link_limit {
        failed.push("第一次接上");
    }
    if m.full.p95 > full_limit {
        failed.push("全部看一遍");
    }
    if m.to_git.p95 > TO_GIT_LIMIT {
        failed.push("AFFiNE 里改一条到提交完");
    }
    if m.to_affine.p95 > TO_AFFINE_LIMIT {
        failed.push("git 里改一条到写进 AFFiNE");
    }
    if m.comments.p95 > COMMENTS_LIMIT {
        failed.push("看一遍评论");
    }
    if grew.is_some_and(|g| g > MEMORY_LIMIT_MB as f64) {
        failed.push("跟着同步多用的内存");
    }
    for f in &failed {
        eprintln!("{f}超过门槛");
    }
    Ok(if check && !failed.is_empty() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

struct Measured {
    link: Duration,
    full: Percentiles,
    to_git: Percentiles,
    to_affine: Percentiles,
    post_comments: Duration,
    comments: Percentiles,
    rss_before: Option<u64>,
    rss_idle: Option<u64>,
    idle_wait: Duration,
}

impl Measured {
    fn grew_mb(&self) -> Option<f64> {
        Some((self.rss_idle? as f64 - self.rss_before? as f64) / 1024.0)
    }
}

/// 跟着同步时每做完一次同步告诉测量的一方：什么时候、写了几个提交、往 AFFiNE 写了几条。
#[derive(Debug)]
enum Done {
    Synced {
        at: Instant,
        full: bool,
        commits: usize,
        to_affine: usize,
    },
    Failed(String),
}

async fn measure(
    root: &std::path::Path,
    repo: &ContextRepo,
    store: &Store,
    ids: &[Vec<ItemId>],
    author: &Author,
    edits: usize,
) -> Result<Measured> {
    tcms_index::reindex(repo, store, false).await?;
    let fake = Rc::new(RefCell::new(Fake::default()));
    {
        let ws = Doc::new();
        set_workspace_name(&ws, "基准");
        fake.borrow_mut().docs.insert(WS.to_owned(), ws);
    }
    let (events, rx) = mpsc::unbounded_channel();
    let mut shared = Shared {
        fake: fake.clone(),
        events: rx,
    };
    let cx = Context {
        repo,
        store,
        workspace: WS,
        actor: "基准",
        remote: None,
        author,
    };

    // 第一次接上。
    let scope: Scope = SCOPE.parse().map_err(anyhow::Error::msg)?;
    let t = Instant::now();
    let r = link(&cx, &mut shared, &scope, None).await?;
    let link_took = t.elapsed();
    let linked = r.docs.iter().filter(|d| d.state == "linked").count();
    ensure!(
        linked == ids.len(),
        "接上了 {linked} 篇，应该是 {}",
        ids.len()
    );

    // 全部看一遍、两边都没变。
    let mut full = Vec::with_capacity(FULL_RUNS);
    for _ in 0..FULL_RUNS {
        let t = Instant::now();
        let r = sync(&cx, &mut shared, &[]).await?;
        full.push(t.elapsed());
        ensure!(
            r.commits.is_empty() && r.docs.iter().all(|d| d.state == "unchanged"),
            "两边都没变时同步出了变化"
        );
    }

    let doc_ids: HashMap<String, String> = store
        .affine_docs()
        .await?
        .into_iter()
        .map(|d| (d.path, d.doc_id))
        .collect();
    let rss_before = settle(Duration::from_secs(3)).await.0;

    // 跟着同步。
    let env = FollowEnv {
        repo: root.to_owned(),
        store: store.clone(),
        workspace: WS.to_owned(),
        actor: "基准".to_owned(),
        remote: None,
        author: author.clone(),
        link: FollowLink::default(),
        public_url: None,
    };
    let opts = FollowOptions {
        quiet: Duration::ZERO,
        max_wait: Duration::ZERO,
        full_every: Duration::from_secs(3600),
        ..FollowOptions::default()
    };
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let (done_tx, mut done) = mpsc::unbounded_channel();
    let follower = follow(&env, &mut shared, opts, stopped, |ev| {
        let msg = match ev {
            FollowEvent::Synced(job, r) => Done::Synced {
                at: Instant::now(),
                full: *job == Job::Full,
                commits: r.commits.len(),
                to_affine: r.docs.iter().map(|d| d.to_affine.total()).sum(),
            },
            FollowEvent::Failed(e) => Done::Failed(e.to_string()),
            _ => return,
        };
        let _ = done_tx.send(msg);
    });
    let script = async {
        let r = drive(&fake, &events, &mut done, repo, store, ids, &doc_ids, edits).await;
        let _ = stop.send(true);
        r
    };
    let (_, driven) = tokio::join!(follower, script);
    let (to_git, to_affine, rss_idle, idle_wait) = driven?;

    // 提议显示成评论：每篇文档上两条。
    let (_, secret) = store
        .create_token(
            &NewToken {
                name: "基准".to_owned(),
                role: "bench".to_owned(),
                team: None,
                projects: Vec::new(),
                expires_in_days: None,
            },
            "基准",
        )
        .await?;
    let who = store
        .identify(&secret)
        .await?
        .context("刚发的令牌认不出来")?;
    let targets: Vec<&ItemId> = ids.iter().flat_map(|f| f.iter().take(2)).collect();
    for (n, item) in targets.iter().cycle().take(PROPOSALS).enumerate() {
        let p = NewProposal {
            action: Action::Update,
            item_id: Some(item.as_str().to_owned()),
            base_revision: Some(1),
            scope: SCOPE.to_owned(),
            kind: "fact".to_owned(),
            title: format!("提议 {n}"),
            body: "改成这样。".to_owned(),
            tags: Vec::new(),
            review_by: None,
            reason: "基准里的提议，理由写得长一点，看评论里截到多少。".repeat(5),
            evidence: Vec::new(),
            conflicts: json!([]),
            content_hash: format!("bench-{n}"),
            redacted: 0,
        };
        let Proposed::New(_) = store
            .create_proposal(&who, &p, PROPOSALS as i64 + 1)
            .await?
        else {
            bail!("提议没建成");
        };
    }
    let t = Instant::now();
    let r = sync_comments(store, &mut shared, WS, Some("https://ctx.example.com")).await?;
    let post_comments = t.elapsed();
    ensure!(
        r.posted.len() == PROPOSALS && r.errors.is_empty(),
        "评论没发全：{r:?}"
    );
    let mut comments = Vec::with_capacity(COMMENT_RUNS);
    for _ in 0..COMMENT_RUNS {
        let t = Instant::now();
        let r = sync_comments(store, &mut shared, WS, Some("https://ctx.example.com")).await?;
        comments.push(t.elapsed());
        ensure!(r.is_empty(), "都发过了还有事做：{r:?}");
    }

    Ok(Measured {
        link: link_took,
        full: percentiles(full),
        to_git,
        to_affine,
        post_comments,
        comments: percentiles(comments),
        rss_before,
        rss_idle,
        idle_wait,
    })
}

/// 跟着同步时的测量：先等第一次全部看一遍；AFFiNE 里改 `edits` 次、git 里改 `edits` 次，一次做完再改下一次；
/// 最后等物理内存稳下来。
#[allow(clippy::too_many_arguments)]
async fn drive(
    fake: &Rc<RefCell<Fake>>,
    events: &mpsc::UnboundedSender<Event>,
    done: &mut mpsc::UnboundedReceiver<Done>,
    repo: &ContextRepo,
    store: &Store,
    ids: &[Vec<ItemId>],
    doc_ids: &HashMap<String, String>,
    edits: usize,
) -> Result<(Percentiles, Percentiles, Option<u64>, Duration)> {
    wait(done, |full, _, _| full).await?;
    let files = ids.len();
    let mut versions: Vec<Vec<usize>> = ids.iter().map(|f| vec![1; f.len()]).collect();

    // AFFiNE 里改一条：改完发广播，等它写进 git。
    let mut to_git = Vec::with_capacity(edits);
    for n in 0..edits {
        let file = (n * 7) % files;
        let item = n % ids[file].len();
        let doc_id = &doc_ids[&path(file)];
        {
            let fake = fake.borrow();
            let doc = fake.doc(doc_id)?;
            let page = tcms_affine::read(doc)?;
            let title = format!("条目 {file}-{item}");
            let heading = split(&page.blocks)
                .into_iter()
                .find_map(|s| match s {
                    Section::Item(i) if i.title == title => i.heading_id.clone(),
                    _ => None,
                })
                .with_context(|| format!("AFFiNE 里没有「{title}」"))?;
            versions[file][item] += 1;
            let body = item_body(file, item, versions[file][item]);
            tcms_affine::update_item(doc, &heading, &title, &to_blocks(&body), &mut RandomIds)?;
        }
        let t = Instant::now();
        events.send(Event::Changed {
            workspace: WS.to_owned(),
            doc: doc_id.clone(),
        })?;
        let at = wait(done, |full, commits, _| !full && commits > 0).await?;
        to_git.push(at - t);
        quiet(done).await?;
    }

    // git 里改一条：提交、更新索引（发变更通知），等它写进 AFFiNE。
    let mut to_affine = Vec::with_capacity(edits);
    for n in 0..edits {
        let file = (n * 11 + 3) % files;
        let item = (n + 5) % ids[file].len();
        versions[file][item] += 1;
        let text = file_text(file, &ids[file], &versions[file]);
        let t = Instant::now();
        repo.commit(
            &[FileEdit {
                path: path(file),
                content: Some(text.into_bytes()),
            }],
            "git 里改一条",
            &Author::default(),
        )?;
        tcms_index::reindex(repo, store, false).await?;
        let at = wait(done, |full, _, to_affine| !full && to_affine > 0).await?;
        to_affine.push(at - t);
        quiet(done).await?;
    }

    let (rss_idle, idle_wait) = settle(Duration::from_secs(5)).await;
    Ok((
        percentiles(to_git),
        percentiles(to_affine),
        rss_idle,
        idle_wait,
    ))
}

/// 等到一次 `ok(全部看一遍, 提交数, 写进 AFFiNE 的条数)` 的同步做完，返回做完的时间。
async fn wait(
    done: &mut mpsc::UnboundedReceiver<Done>,
    ok: impl Fn(bool, usize, usize) -> bool,
) -> Result<Instant> {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        match tokio::time::timeout_at(deadline, done.recv()).await {
            Err(_) => bail!("等了 {} 秒，同步没做完", WAIT.as_secs()),
            Ok(None) => bail!("跟着同步停了"),
            Ok(Some(Done::Failed(e))) => bail!("跟着同步出错：{e}"),
            Ok(Some(Done::Synced {
                at,
                full,
                commits,
                to_affine,
            })) => {
                if ok(full, commits, to_affine) {
                    return Ok(at);
                }
            }
        }
    }
}

/// 等跟着同步把手上的做完（自己的提交会让它再看一遍那篇），200 毫秒里没有新的同步就算完。
async fn quiet(done: &mut mpsc::UnboundedReceiver<Done>) -> Result<()> {
    loop {
        match tokio::time::timeout(Duration::from_millis(200), done.recv()).await {
            Err(_) => return Ok(()),
            Ok(None) => bail!("跟着同步停了"),
            Ok(Some(Done::Failed(e))) => bail!("跟着同步出错：{e}"),
            Ok(Some(Done::Synced { .. })) => {}
        }
    }
}

/// 等这个进程的物理内存稳下来：至少等 `min`，之后 3 秒里降得不到 1 MB 就算稳了，最多等 20 秒。
async fn settle(min: Duration) -> (Option<u64>, Duration) {
    const EVERY: Duration = Duration::from_millis(500);
    const WINDOW: usize = 6;
    const MAX: Duration = Duration::from_secs(20);
    let t = Instant::now();
    let mut samples = Vec::new();
    loop {
        let Some(kb) = rss_kb(None) else {
            return (None, t.elapsed());
        };
        samples.push(kb);
        let steady =
            samples.len() > WINDOW && samples[samples.len() - 1 - WINDOW].saturating_sub(kb) < 1024;
        if (t.elapsed() >= min && steady) || t.elapsed() >= MAX {
            return (Some(kb), t.elapsed());
        }
        tokio::time::sleep(EVERY).await;
    }
}
