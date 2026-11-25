//! 一直跟着同步（`docs/affine-sync.md` 7.4）：`tcms serve` 里跟着跑，也可以 `tcms affine follow` 在前台跑。
//!
//! 什么时候同步：
//! - AFFiNE 里有人改了一篇（同步协议推来的广播）：等这篇停下 [`FollowOptions::quiet`] 没人再改再同步，
//!   一直有人在改的话最多等 [`FollowOptions::max_wait`]。工作区根文档、文件夹树改了也一样（新建文档、移进回收站、挪文件夹）；
//! - git 里变了：每 [`FollowOptions::git_poll`] 看一次 HEAD，索引发了变更通知（批准、改单条、从远端拉下来）时马上看，
//!   和上次同步完不一样的文档马上同步；
//! - 刚开始、断线重连以后、每 [`FollowOptions::full_every`]：全部看一遍，补上漏掉的广播；
//! - 出错了：[`FollowOptions::retry`] 以后再来，没同步成的留着。
//!
//! 每次同步都看一遍新文件、新文档和回收站；接上的文档只看要看的那几篇，两边都没变的不拿提交队列的锁
//! （[`crate::bridge`] 的“同步一次”）。
//!
//! 和审核页共用一个 [`FollowLink`]：审核页从它看现在怎么样了（连没连着、上次同步、出的错），
//! 人在审核页处理完等人处理的、让停着的文档接着做以后，通过它让这边马上同步那几篇。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use tcms_git::{Author, ContextRepo, ObjectId, Remote};
use tcms_store::{PgListener, Store};
use tokio::sync::{Notify, watch};
use tokio::time::Instant;

use crate::bridge::{Context, Error, SyncReport, folders_doc_id, git_changes, sync_docs};
use crate::server::{Affine, Event};

/// 跟着同步的几个时间。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowOptions {
    /// AFFiNE 里改完停下多久没人再改才同步。
    pub quiet: Duration,
    /// 一直有人在改的话，从第一次改起最多等多久。
    pub max_wait: Duration,
    /// 多久看一次 git 的 HEAD（有变更通知时马上看）。
    pub git_poll: Duration,
    /// 多久全部看一遍。
    pub full_every: Duration,
    /// 出错以后多久再来。
    pub retry: Duration,
}

impl Default for FollowOptions {
    fn default() -> Self {
        Self {
            quiet: Duration::from_secs(10),
            max_wait: Duration::from_secs(120),
            git_poll: Duration::from_secs(30),
            full_every: Duration::from_secs(300),
            retry: Duration::from_secs(30),
        }
    }
}

/// 要做的一次同步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// 全部看一遍。
    Full,
    /// 接上的文档只看这几篇（文档 id；不是接上的文档的 id 不起作用）。新文件、新文档和回收站每次都看。
    Docs(HashSet<String>),
}

/// 什么时候同步什么。只管时间和文档 id，不连别的，测试直接用。
#[derive(Debug)]
pub struct Schedule {
    opts: FollowOptions,
    /// AFFiNE 里改过、还在等它停下来的：文档 id →（第一次改，最后一次改）。
    waiting: HashMap<String, (Instant, Instant)>,
    /// 到时候了、等着同步的。
    due: HashSet<String>,
    /// 要看一遍新文件、新文档和回收站。
    scan: bool,
    /// 要全部看一遍。
    full: bool,
    next_full: Instant,
    /// 出错以后，这之前不同步。
    retry_at: Option<Instant>,
}

impl Schedule {
    /// 开始：先全部看一遍。
    pub fn new(opts: FollowOptions, now: Instant) -> Self {
        Self {
            next_full: now + opts.full_every,
            opts,
            waiting: HashMap::new(),
            due: HashSet::new(),
            scan: false,
            full: true,
            retry_at: None,
        }
    }

    /// AFFiNE 里有人改了 `doc`。
    pub fn affine_changed(&mut self, doc: &str, now: Instant) {
        self.waiting
            .entry(doc.to_owned())
            .and_modify(|(_, last)| *last = now)
            .or_insert((now, now));
    }

    /// git 里变了：`docs` 马上同步；`added` 是整个接上的范围里有新文件。
    pub fn git_changed(&mut self, docs: impl IntoIterator<Item = String>, added: bool) {
        self.due.extend(docs);
        self.scan |= added;
    }

    /// 断线重连了：断开的时候收不到广播，全部看一遍。
    pub fn reconnected(&mut self) {
        self.full = true;
    }

    fn ready_at(&self, first: Instant, last: Instant) -> Instant {
        (last + self.opts.quiet).min(first + self.opts.max_wait)
    }

    /// 现在该同步什么；没到时候是 `None`。
    pub fn take(&mut self, now: Instant) -> Option<Job> {
        if self.retry_at.is_some_and(|t| now < t) {
            return None;
        }
        self.retry_at = None;
        let ready: Vec<String> = self
            .waiting
            .iter()
            .filter(|(_, (first, last))| now >= self.ready_at(*first, *last))
            .map(|(doc, _)| doc.clone())
            .collect();
        for doc in ready {
            self.waiting.remove(&doc);
            self.due.insert(doc);
        }
        if self.full || now >= self.next_full {
            // 还在改的（waiting）留着，停下来以后再同步一次。
            self.full = false;
            self.next_full = now + self.opts.full_every;
            self.due.clear();
            self.scan = false;
            return Some(Job::Full);
        }
        if self.due.is_empty() && !self.scan {
            return None;
        }
        self.scan = false;
        Some(Job::Docs(std::mem::take(&mut self.due)))
    }

    /// 下一次该醒的时候（有事要做时是 `now`）。
    pub fn wake_at(&self, now: Instant) -> Instant {
        let mut at = self.next_full;
        if self.full || self.scan || !self.due.is_empty() {
            at = now;
        }
        for (first, last) in self.waiting.values() {
            at = at.min(self.ready_at(*first, *last));
        }
        match self.retry_at {
            Some(r) => at.max(r),
            None => at,
        }
    }

    /// 这次同步出错了：过 [`FollowOptions::retry`] 再来，没同步成的留着。
    pub fn failed(&mut self, job: Job, now: Instant) {
        self.retry_at = Some(now + self.opts.retry);
        match job {
            Job::Full => self.full = true,
            Job::Docs(docs) => {
                self.due.extend(docs);
                self.scan = true;
            }
        }
    }
}

/// 和 AFFiNE 的连接。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Connection {
    /// 这个进程没跟着同步。
    #[default]
    Off,
    /// 还没连上（连不上时隔一会儿再试）。
    Connecting,
    Connected,
    /// 断了，后台在重连。
    Disconnected,
}

/// 跟着同步现在的样子。只在内存里，服务重启以后从头记。时间都是 Unix 毫秒。
#[derive(Debug, Clone, Default, Serialize)]
pub struct FollowState {
    pub connection: Connection,
    /// 跟着同步的工作区。
    pub workspace: String,
    /// 连接变成现在这样的时候。
    pub since_ms: Option<u64>,
    /// 最近一次同步完（不管有没有改动）。
    pub last_sync_ms: Option<u64>,
    /// 最近一次同步往 git 或 AFFiNE 写了东西。
    pub last_change_ms: Option<u64>,
    /// 最近一次出的错；之后同步成了就清掉。
    pub error: Option<String>,
    pub error_ms: Option<u64>,
}

/// 排着等同步的（人在审核页处理完、还没同步的）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Queued {
    /// 马上同步的文档 id（跟着同步的一方拿走以后就不在这里了）。
    pub docs: Vec<String>,
    /// 接着做的文件（停着的，人看过了）。同步成了才去掉。
    pub resume: Vec<String>,
}

#[derive(Debug, Default)]
struct LinkInner {
    state: Mutex<FollowState>,
    queued: Mutex<(HashSet<String>, HashSet<String>)>,
    notify: Notify,
}

/// 跟着同步和审核页共用的：现在怎么样了，和“这几篇马上同步”。克隆出来的是同一个。
#[derive(Debug, Clone, Default)]
pub struct FollowLink(Arc<LinkInner>);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl FollowLink {
    /// 现在的样子。
    pub fn state(&self) -> FollowState {
        locked(&self.0.state).clone()
    }

    /// 排着等同步的。
    pub fn queued(&self) -> Queued {
        let q = locked(&self.0.queued);
        let mut docs: Vec<String> = q.0.iter().cloned().collect();
        let mut resume: Vec<String> = q.1.iter().cloned().collect();
        docs.sort();
        resume.sort();
        Queued { docs, resume }
    }

    /// 这篇文档马上同步一次（人在审核页处理完它的等人处理的条目以后）。没在跟着同步时什么都不会发生。
    pub fn sync_now(&self, doc_id: &str) {
        locked(&self.0.queued).0.insert(doc_id.to_owned());
        self.0.notify.notify_one();
    }

    /// 这个停着的文件接着做（和 `tcms affine sync --resume <路径>` 一样）。
    pub fn resume(&self, path: &str) {
        locked(&self.0.queued).1.insert(path.to_owned());
        self.0.notify.notify_one();
    }

    /// 在连 AFFiNE；`error` 是上一次没连上的原因。
    pub fn connecting(&self, workspace: &str, error: Option<String>) {
        let mut s = locked(&self.0.state);
        if s.connection != Connection::Connecting {
            s.since_ms = Some(now_ms());
        }
        s.connection = Connection::Connecting;
        s.workspace = workspace.to_owned();
        if let Some(e) = error {
            s.error = Some(e);
            s.error_ms = Some(now_ms());
        }
    }

    fn connection(&self, c: Connection) {
        let mut s = locked(&self.0.state);
        if s.connection != c {
            s.connection = c;
            s.since_ms = Some(now_ms());
        }
    }

    fn synced(&self, changed: bool) {
        let mut s = locked(&self.0.state);
        let now = now_ms();
        s.last_sync_ms = Some(now);
        if changed {
            s.last_change_ms = Some(now);
        }
        s.error = None;
        s.error_ms = None;
    }

    fn failed(&self, error: &Error) {
        let mut s = locked(&self.0.state);
        s.error = Some(error.to_string());
        s.error_ms = Some(now_ms());
    }

    /// 拿走要马上同步的文档。
    fn take_docs(&self) -> HashSet<String> {
        std::mem::take(&mut locked(&self.0.queued).0)
    }

    /// 要接着做的文件（还没同步成的）。
    fn resuming(&self) -> Vec<String> {
        let mut v: Vec<String> = locked(&self.0.queued).1.iter().cloned().collect();
        v.sort();
        v
    }

    /// 这几个文件接着做完了。
    fn resumed(&self, paths: &[String]) {
        let mut q = locked(&self.0.queued);
        for p in paths {
            q.1.remove(p);
        }
    }
}

/// 跟着同步要的东西。每次同步都重新打开上下文仓库，看得到别处做的提交。
pub struct FollowEnv {
    pub repo: PathBuf,
    pub store: Store,
    /// AFFiNE 工作区 id。
    pub workspace: String,
    /// 审计里记的“谁”。
    pub actor: String,
    pub remote: Option<Remote>,
    /// 从远端拉的时候合并提交的作者。
    pub author: Author,
    /// 和审核页共用的。
    pub link: FollowLink,
}

/// 跟着同步时发生的事，交给调用的一方（记日志）。
#[derive(Debug)]
pub enum FollowEvent<'a> {
    Synced(&'a Job, &'a SyncReport),
    Failed(&'a Error),
    Disconnected(&'a str),
    Reconnected,
}

/// 一直跟着同步，直到 `stop` 变成 `true`（或者发送的一方没了）。正在做的那次同步做完才停。
pub async fn follow(
    env: &FollowEnv,
    affine: &mut impl Affine,
    opts: FollowOptions,
    mut stop: watch::Receiver<bool>,
    mut on: impl FnMut(FollowEvent<'_>),
) {
    let mut schedule = Schedule::new(opts.clone(), Instant::now());
    let mut listener = env.store.listen().await.ok();
    let mut git_tick = tokio::time::interval(opts.git_poll);
    git_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 上次看 git 时的 HEAD；没变就不用再比。
    let mut seen: Option<Option<ObjectId>> = None;
    env.link.connection(Connection::Connected);
    loop {
        if *stop.borrow() {
            return;
        }
        if let Some(job) = schedule.take(Instant::now()) {
            // 人在审核页让接着做的文件，同步成了才去掉。
            let resuming = env.link.resuming();
            match run_job(env, affine, &job, &resuming).await {
                Ok(report) => {
                    env.link.resumed(&resuming);
                    env.link.synced(changed(&report));
                    on(FollowEvent::Synced(&job, &report));
                }
                Err(e) => {
                    env.link.failed(&e);
                    on(FollowEvent::Failed(&e));
                    schedule.failed(job, Instant::now());
                }
            }
            continue;
        }
        let wake = schedule.wake_at(Instant::now());
        let look_at_git = tokio::select! {
            r = stop.changed() => {
                if r.is_err() {
                    return;
                }
                false
            }
            ev = affine.next_event() => {
                match ev {
                    Event::Changed { workspace, doc } if workspace == env.workspace => {
                        schedule.affine_changed(&doc, Instant::now());
                    }
                    Event::Changed { .. } => {}
                    Event::Disconnected(reason) => {
                        env.link.connection(Connection::Disconnected);
                        on(FollowEvent::Disconnected(&reason));
                    }
                    Event::Reconnected => {
                        env.link.connection(Connection::Connected);
                        schedule.reconnected();
                        on(FollowEvent::Reconnected);
                    }
                }
                false
            }
            () = env.link.0.notify.notified() => {
                let docs = env.link.take_docs();
                schedule.git_changed(docs, !env.link.resuming().is_empty());
                false
            }
            r = notice(&mut listener) => {
                if r.is_err() {
                    // 下一次定时看 git 时重新订阅。
                    listener = None;
                }
                true
            }
            _ = git_tick.tick() => {
                if listener.is_none() {
                    listener = env.store.listen().await.ok();
                }
                true
            }
            () = tokio::time::sleep_until(wake) => false,
        };
        if look_at_git && let Err(e) = check_git(env, &mut schedule, &mut seen).await {
            env.link.failed(&e);
            on(FollowEvent::Failed(&e));
        }
    }
}

/// 这次同步往哪边写了东西。
fn changed(report: &SyncReport) -> bool {
    !report.commits.is_empty()
        || report
            .docs
            .iter()
            .any(|d| d.to_git.total() + d.to_affine.total() > 0 || d.state == "unlinked")
}

async fn notice(listener: &mut Option<PgListener>) -> Result<(), tcms_store::Error> {
    match listener {
        Some(l) => l.recv().await.map(|_| ()).map_err(Into::into),
        None => std::future::pending().await,
    }
}

/// HEAD 变了的话，看哪些文档在 git 里变了、有没有新文件。
async fn check_git(
    env: &FollowEnv,
    schedule: &mut Schedule,
    seen: &mut Option<Option<ObjectId>>,
) -> Result<(), Error> {
    let repo = ContextRepo::open(&env.repo)?;
    let head = repo.head()?;
    if *seen == Some(head) {
        return Ok(());
    }
    let docs = env.store.affine_docs().await?;
    let scopes = env.store.affine_scopes().await?;
    let (changed, added) = git_changes(&repo, &env.workspace, &docs, &scopes)?;
    schedule.git_changed(changed, added);
    *seen = Some(head);
    Ok(())
}

async fn run_job(
    env: &FollowEnv,
    affine: &mut impl Affine,
    job: &Job,
    resume: &[String],
) -> Result<SyncReport, Error> {
    let repo = ContextRepo::open(&env.repo)?;
    let cx = Context {
        repo: &repo,
        store: &env.store,
        workspace: &env.workspace,
        actor: &env.actor,
        remote: env.remote.as_ref(),
        author: &env.author,
    };
    let only = match job {
        Job::Full => None,
        Job::Docs(docs) => Some(docs),
    };
    let report = sync_docs(&cx, affine, resume, only, false).await?;
    // 新接上的文档也要收广播（取过的已经在收了）。
    let mut ids = vec![env.workspace.clone(), folders_doc_id(&env.workspace)];
    ids.extend(
        env.store
            .affine_docs()
            .await?
            .into_iter()
            .filter(|d| d.workspace_id == env.workspace)
            .map(|d| d.doc_id),
    );
    affine.watch(&env.workspace, &ids).await?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> FollowOptions {
        FollowOptions {
            quiet: Duration::from_secs(10),
            max_wait: Duration::from_secs(60),
            git_poll: Duration::from_secs(30),
            full_every: Duration::from_secs(300),
            retry: Duration::from_secs(30),
        }
    }

    fn docs(ids: &[&str]) -> Job {
        Job::Docs(ids.iter().map(|s| (*s).to_owned()).collect())
    }

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// 刚开始先全部看一遍，然后没事做，下次醒在下一次全部看一遍的时候。
    #[test]
    fn starts_with_a_full_sync() {
        let t0 = Instant::now();
        let mut sch = Schedule::new(opts(), t0);
        assert_eq!(sch.wake_at(t0), t0);
        assert_eq!(sch.take(t0), Some(Job::Full));
        assert_eq!(sch.take(t0), None);
        assert_eq!(sch.wake_at(t0), t0 + s(300));
        assert_eq!(sch.take(t0 + s(300)), Some(Job::Full));
    }

    /// AFFiNE 里改了：停下 10 秒才同步；一直在改的话最多等 60 秒。
    #[test]
    fn affine_edits_wait_until_quiet() {
        let t0 = Instant::now();
        let mut sch = Schedule::new(opts(), t0);
        sch.take(t0);
        sch.affine_changed("a", t0);
        assert_eq!(sch.wake_at(t0), t0 + s(10));
        assert_eq!(sch.take(t0 + s(9)), None);
        sch.affine_changed("a", t0 + s(9));
        assert_eq!(sch.take(t0 + s(10)), None);
        assert_eq!(sch.take(t0 + s(19)), Some(docs(&["a"])));
        assert_eq!(sch.take(t0 + s(19)), None);

        // 一直在改：每 5 秒改一次，第 60 秒到点。
        let t1 = t0 + s(100);
        for i in 0..12 {
            sch.affine_changed("b", t1 + s(5 * i));
            assert_eq!(sch.take(t1 + s(5 * i)), None, "第 {i} 次");
        }
        assert_eq!(sch.wake_at(t1 + s(55)), t1 + s(60));
        assert_eq!(sch.take(t1 + s(60)), Some(docs(&["b"])));
    }

    /// git 里变了马上同步；有新文件时空的那种也同步（看新文件）。
    #[test]
    fn git_changes_are_due_now() {
        let t0 = Instant::now();
        let mut sch = Schedule::new(opts(), t0);
        sch.take(t0);
        sch.git_changed(["x".to_owned()], false);
        assert_eq!(sch.wake_at(t0 + s(1)), t0 + s(1));
        assert_eq!(sch.take(t0 + s(1)), Some(docs(&["x"])));
        sch.git_changed([], false);
        assert_eq!(sch.take(t0 + s(2)), None);
        sch.git_changed([], true);
        assert_eq!(sch.take(t0 + s(2)), Some(docs(&[])));
    }

    /// 出错了：30 秒以后再来，没同步成的留着，新来的一起做。
    #[test]
    fn failures_are_retried_later() {
        let t0 = Instant::now();
        let mut sch = Schedule::new(opts(), t0);
        let job = sch.take(t0).unwrap();
        sch.failed(job, t0);
        assert_eq!(sch.take(t0 + s(29)), None);
        assert_eq!(sch.wake_at(t0 + s(1)), t0 + s(30));
        assert_eq!(sch.take(t0 + s(30)), Some(Job::Full));

        sch.git_changed(["x".to_owned()], false);
        let job = sch.take(t0 + s(31)).unwrap();
        sch.failed(job, t0 + s(31));
        sch.git_changed(["y".to_owned()], false);
        assert_eq!(sch.take(t0 + s(40)), None);
        assert_eq!(sch.take(t0 + s(61)), Some(docs(&["x", "y"])));
    }

    /// 重连以后全部看一遍；全部看的时候还在改的那篇留着，停下来以后再同步。
    #[test]
    fn reconnect_means_full_and_keeps_waiting_docs() {
        let t0 = Instant::now();
        let mut sch = Schedule::new(opts(), t0);
        sch.take(t0);
        sch.affine_changed("a", t0 + s(1));
        sch.git_changed(["x".to_owned()], false);
        sch.reconnected();
        assert_eq!(sch.take(t0 + s(2)), Some(Job::Full));
        assert_eq!(sch.take(t0 + s(2)), None);
        assert_eq!(sch.take(t0 + s(11)), Some(docs(&["a"])));
    }
}
