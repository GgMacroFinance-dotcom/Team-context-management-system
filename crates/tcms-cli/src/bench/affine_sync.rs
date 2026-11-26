//! `tcms bench affine`：和 AFFiNE 同步的基准（`docs/affine-sync.md` 的 A5）。上下文仓库在临时目录里，
//! PostgreSQL 用 TCMS_DATABASE_URL 指的库里一个临时 schema，AFFiNE 换成这个进程里的假 AFFiNE（内存里的
//! Yjs 文档，收发更新、存评论），量的是本系统这一侧：
//!
//! - 第一次接上：一个范围 1 千条（50 个文件），在 AFFiNE 里建文件夹和 50 篇文档、记下底稿；
//! - 全部看一遍、两边都没变（跟着同步时每 5 分钟一次）：接上以后第一次每篇都取，单独报；之后 AFFiNE 说最后改的
//!   时间没变的不取（`docs/affine-sync.md` 7.11），量的是这种；
//! - 跟着同步（`follow`，等人停手的 10 秒改成 0）：AFFiNE 里改一条，从收到广播到 git 里提交完；
//!   git 里改一条，从提交、更新索引到写进 AFFiNE；
//! - 跟着同步、空闲时比开始跟着同步之前多用的物理内存；
//! - 提议显示成评论：给 100 条提议发评论，再看一遍（都发过了，只取回复）。
//!
//! 连真的 AFFiNE 时还要加上网络往返（同步协议取、推一篇各一次），假 AFFiNE 量不到。`--real` 换成真的 AFFiNE
//! （`TCMS_AFFINE_URL`、`TCMS_AFFINE_EMAIL`、`TCMS_AFFINE_PASSWORD`，和 `tcms serve` 一样）：每跑一次在里面新建一个
//! 工作区（不删），“人在 AFFiNE 里改”用另一条同步连接推一段修改（和网页编辑器推的一样是 Yjs 更新），
//! 收广播、取、推都走网络。只该对着测试用的 AFFiNE 跑。CI 里没有 AFFiNE，不跑。

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

use affine_sync_client::{Client, Comment, Config, Error as ClientError, Intent, SyncConnection};
use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};
use tcms_affine::workspace::set_workspace_name;
use tcms_affine::{
    Affine, Context, Event, FollowEnv, FollowEvent, FollowLink, FollowOptions, Job, RandomIds,
    Section, Server, follow, link, split, sync, sync_comments, to_blocks,
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
/// 全部看一遍、两边都没变（1 千条），p95 的门槛（条数不同时按比例换算）。接上以后第一次（每篇都取）也用它。
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
    /// 每篇文档“最后改的时间”：改一次加一（真 AFFiNE 是毫秒时间戳）。
    stamps: HashMap<String, i64>,
    clock: i64,
    comments: HashMap<String, Vec<Comment>>,
    count: usize,
}

impl Fake {
    fn doc(&self, id: &str) -> Result<&Doc> {
        self.docs
            .get(id)
            .with_context(|| format!("假 AFFiNE 里没有 {id}"))
    }

    fn touch(&mut self, doc: &str) {
        self.clock += 1;
        self.stamps.insert(doc.to_owned(), self.clock);
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
        fake.touch(doc);
        let d = fake.docs.entry(doc.to_owned()).or_default();
        d.transact_mut()
            .apply_update(update)
            .map_err(|e| ClientError::Protocol(e.to_string()))
    }

    async fn doc_timestamps(
        &mut self,
        _ws: &str,
    ) -> Result<Option<HashMap<String, i64>>, ClientError> {
        Ok(Some(self.fake.borrow().stamps.clone()))
    }

    async fn editor(&mut self, _ws: &str, doc: &str) -> Result<Option<String>, ClientError> {
        Ok(self
            .fake
            .borrow()
            .docs
            .contains_key(doc)
            .then(|| "基准".to_owned()))
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

/// 模拟人在 AFFiNE 里把一条改成 `body`，改完发出去；返回发出去之前的时间。
trait Human {
    async fn edit(&mut self, doc_id: &str, title: &str, body: &str) -> Result<Instant>;
}

/// 在 `doc` 里把标题是 `title` 的那一条的正文换成 `body`。
fn edit_item(doc: &Doc, title: &str, body: &str) -> Result<()> {
    let page = tcms_affine::read(doc)?;
    let heading = split(&page.blocks)
        .into_iter()
        .find_map(|s| match s {
            Section::Item(i) if i.title == title => i.heading_id.clone(),
            _ => None,
        })
        .with_context(|| format!("AFFiNE 里没有「{title}」"))?;
    tcms_affine::update_item(doc, &heading, title, &to_blocks(body), &mut RandomIds)?;
    Ok(())
}

/// 假 AFFiNE：直接改内存里的文档，再发一个广播。
struct FakeHuman {
    fake: Rc<RefCell<Fake>>,
    events: mpsc::UnboundedSender<Event>,
}

impl Human for FakeHuman {
    async fn edit(&mut self, doc_id: &str, title: &str, body: &str) -> Result<Instant> {
        edit_item(self.fake.borrow().doc(doc_id)?, title, body)?;
        self.fake.borrow_mut().touch(doc_id);
        let t = Instant::now();
        self.events.send(Event::Changed {
            workspace: WS.to_owned(),
            doc: doc_id.to_owned(),
        })?;
        Ok(t)
    }
}

/// 真 AFFiNE：另一条同步连接，取来改了再推上去。第一次改的时候才连：文档都建好以后再加入工作区，
/// 不然新建文档以后之前的加入作废（`docs/affine-sync.md` 7.2）。
struct RealHuman {
    config: Config,
    ws: String,
    conn: Option<SyncConnection>,
}

impl Human for RealHuman {
    async fn edit(&mut self, doc_id: &str, title: &str, body: &str) -> Result<Instant> {
        if self.conn.is_none() {
            let conn = Client::new(self.config.clone())?.sync().await?;
            self.conn = Some(conn);
        }
        let conn = self.conn.as_ref().context("没连上")?;
        let loaded = conn
            .load_doc(&self.ws, doc_id, None)
            .await?
            .with_context(|| format!("AFFiNE 里没有文档 {doc_id}"))?;
        let doc = Doc::new();
        doc.transact_mut()
            .apply_update(Update::decode_v1(&loaded.update)?)?;
        let sv = doc.transact().state_vector();
        edit_item(&doc, title, body)?;
        let delta = doc.transact().encode_state_as_update_v1(&sv);
        let t = Instant::now();
        conn.push_update(&self.ws, doc_id, &delta, Intent::Update)
            .await?;
        Ok(t)
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

pub async fn run(items: usize, edits: usize, check: bool, real: bool) -> Result<ExitCode> {
    // 门槛是按假 AFFiNE 定的，连真的时还有网络往返，不比。
    ensure!(
        !(real && check),
        "--real 不能带 --check：门槛是按假 AFFiNE 定的"
    );
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
    let b = Bench {
        root: dir.path(),
        repo: &repo,
        store: &store,
        ids: &ids,
        author: &author,
        edits,
    };
    let result = if real {
        real_affine(&b).await
    } else {
        fake_affine(&b).await
    };
    store.drop_schema().await?;
    let m = result?;

    let link_limit = LINK_LIMIT_1K.mul_f64(items as f64 / 1000.0);
    let full_limit = FULL_LIMIT_1K.mul_f64(items as f64 / 1000.0);
    let limit = |prefix: &str, limit: Duration| {
        if real {
            String::new()
        } else {
            format!("   门槛 {prefix}{:.0} 毫秒", ms(limit))
        }
    };
    let line = |name: &str, p: &Percentiles, l: Duration| {
        println!(
            "  {name}  中位数 {:>7.1} 毫秒  p95 {:>7.1} 毫秒{}",
            ms(p.p50),
            ms(p.p95),
            limit("p95 ", l)
        );
    };
    let backend = if real {
        "真 AFFiNE，TCMS_AFFINE_URL 指的那个"
    } else {
        "假 AFFiNE，在这个进程里"
    };
    println!("和 AFFiNE 同步的基准：{items} 条、{files} 篇文档（{backend}）");
    println!(
        "  第一次接上                     {:>7.1} 毫秒{}",
        ms(m.link),
        limit("", link_limit)
    );
    println!(
        "  第一次全部看一遍（每篇都取）   {:>7.1} 毫秒{}",
        ms(m.first_full),
        limit("", full_limit)
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
        "  物理内存：跟着同步之前 {}，跟着同步、空闲 {:.0} 秒以后 {}，变化 {}{}",
        mb(m.rss_before),
        m.idle_wait.as_secs_f64(),
        mb(m.rss_idle),
        grew.map_or("量不到".to_owned(), |g| format!("{g:+.1} MB")),
        if real {
            String::new()
        } else {
            format!("（多用的门槛 {MEMORY_LIMIT_MB} MB）")
        },
    );
    if real {
        return Ok(ExitCode::SUCCESS);
    }

    let mut failed = Vec::new();
    if m.link > link_limit {
        failed.push("第一次接上");
    }
    if m.first_full > full_limit {
        failed.push("第一次全部看一遍");
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
    /// 接上以后第一次全部看一遍（每篇都取）。
    first_full: Duration,
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

/// 一次基准的数据：上下文仓库、PostgreSQL、造的条目。
struct Bench<'a> {
    root: &'a std::path::Path,
    repo: &'a ContextRepo,
    store: &'a Store,
    ids: &'a [Vec<ItemId>],
    author: &'a Author,
    edits: usize,
}

async fn fake_affine(b: &Bench<'_>) -> Result<Measured> {
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
    let mut human = FakeHuman { fake, events };
    measure(b, &mut shared, WS, &mut human).await
}

async fn real_affine(b: &Bench<'_>) -> Result<Measured> {
    let config = crate::affine::config()?;
    let mut server = Server::connect(config.clone()).await.context("连 AFFiNE")?;
    let ws = server
        .create_workspace("TCMS 基准")
        .await
        .context("在 AFFiNE 里建工作区")?;
    let mut human = RealHuman {
        config,
        ws: ws.clone(),
        conn: None,
    };
    measure(b, &mut server, &ws, &mut human).await
}

async fn measure(
    b: &Bench<'_>,
    affine: &mut impl Affine,
    ws: &str,
    human: &mut impl Human,
) -> Result<Measured> {
    let Bench {
        root,
        repo,
        store,
        ids,
        author,
        ..
    } = *b;
    tcms_index::reindex(repo, store, false).await?;
    let cx = Context {
        repo,
        store,
        workspace: ws,
        actor: "基准",
        remote: None,
        author,
    };

    // 第一次接上。
    let scope: Scope = SCOPE.parse().map_err(anyhow::Error::msg)?;
    let t = Instant::now();
    let r = link(&cx, affine, &scope, None).await?;
    let link_took = t.elapsed();
    let linked = r.docs.iter().filter(|d| d.state == "linked").count();
    ensure!(
        linked == ids.len(),
        "接上了 {linked} 篇，应该是 {}",
        ids.len()
    );

    // 全部看一遍、两边都没变。接上以后第一次还没记 AFFiNE 说的时间，每篇都取。
    let mut full = Vec::with_capacity(FULL_RUNS + 1);
    for _ in 0..=FULL_RUNS {
        let t = Instant::now();
        let r = sync(&cx, affine, &[]).await?;
        full.push(t.elapsed());
        ensure!(
            r.commits.is_empty() && r.docs.iter().all(|d| d.state == "unchanged"),
            "两边都没变时同步出了变化"
        );
    }
    let first_full = full.remove(0);

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
        workspace: ws.to_owned(),
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
    let follower = follow(&env, &mut *affine, opts, stopped, |ev| {
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
        let r = drive(human, &mut done, b, &doc_ids).await;
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
    let r = sync_comments(store, affine, ws, Some("https://ctx.example.com")).await?;
    let post_comments = t.elapsed();
    ensure!(
        r.posted.len() == PROPOSALS && r.errors.is_empty(),
        "评论没发全：{r:?}"
    );
    let mut comments = Vec::with_capacity(COMMENT_RUNS);
    for _ in 0..COMMENT_RUNS {
        let t = Instant::now();
        let r = sync_comments(store, affine, ws, Some("https://ctx.example.com")).await?;
        comments.push(t.elapsed());
        ensure!(r.is_empty(), "都发过了还有事做：{r:?}");
    }

    Ok(Measured {
        link: link_took,
        first_full,
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
async fn drive(
    human: &mut impl Human,
    done: &mut mpsc::UnboundedReceiver<Done>,
    b: &Bench<'_>,
    doc_ids: &HashMap<String, String>,
) -> Result<(Percentiles, Percentiles, Option<u64>, Duration)> {
    let Bench {
        repo,
        store,
        ids,
        edits,
        ..
    } = *b;
    wait(done, |full, _, _| full).await?;
    let files = ids.len();
    let mut versions: Vec<Vec<usize>> = ids.iter().map(|f| vec![1; f.len()]).collect();

    // AFFiNE 里改一条：改完发广播，等它写进 git。
    let mut to_git = Vec::with_capacity(edits);
    for n in 0..edits {
        let file = (n * 7) % files;
        let item = n % ids[file].len();
        let doc_id = &doc_ids[&path(file)];
        versions[file][item] += 1;
        let body = item_body(file, item, versions[file][item]);
        let t = human
            .edit(doc_id, &format!("条目 {file}-{item}"), &body)
            .await?;
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
