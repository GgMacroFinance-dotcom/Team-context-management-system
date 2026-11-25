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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use tcms_git::{Author, ContextRepo, ObjectId, Remote};
use tcms_store::{PgListener, Store};
use tokio::sync::watch;
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
    loop {
        if *stop.borrow() {
            return;
        }
        if let Some(job) = schedule.take(Instant::now()) {
            match run_job(env, affine, &job).await {
                Ok(report) => on(FollowEvent::Synced(&job, &report)),
                Err(e) => {
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
                    Event::Disconnected(reason) => on(FollowEvent::Disconnected(&reason)),
                    Event::Reconnected => {
                        schedule.reconnected();
                        on(FollowEvent::Reconnected);
                    }
                }
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
            on(FollowEvent::Failed(&e));
        }
    }
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
    let report = sync_docs(&cx, affine, &[], only, false).await?;
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
