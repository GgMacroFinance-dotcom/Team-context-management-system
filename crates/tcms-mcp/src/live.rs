//! 服务常驻的一份条目和检索索引，跟着 PostgreSQL 里的变更更新。
//!
//! 索引（`tcms reindex`）写进 PostgreSQL 以后会 `NOTIFY tcms_changes`。服务收到通知、或者每 30 秒，
//! 查一次最大的变更序号；和手上这份不一样就重新取条目、建检索索引，建好了再换上。换的过程中
//! 请求继续用旧的那份，不会等。通知丢了（断线重连期间）也会在下一次定时检查时补上。
//!
//! 取条目和建索引都在一个专门的线程里做（自己的单线程 tokio 运行时、自己的一个数据库连接）。
//! 这样每一份条目和索引的内存都从同一处分配、放掉以后下一次重建接着用；在 tokio 的工作线程里做时，
//! 每次可能落在不同的线程上，glibc 每个线程一块内存区，换几次索引物理内存就涨一截（第 3 步量过：
//! 1 万条换 7 次从约 80 MB 涨到约 240 MB）。

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tcms_search::{Corpus, SearchIndex};
use tcms_store::{PgListener, Store};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::Error;

/// 没有通知时多久查一次。
const POLL: Duration = Duration::from_secs(30);

type Built = Result<SearchIndex, Error>;

pub struct Live {
    store: Store,
    index: RwLock<Arc<SearchIndex>>,
    /// 同一时间只重建一次。
    rebuilding: Mutex<()>,
    /// 交给建索引的线程：每发一个，它取一次条目、建一份索引，从这里送回来。
    builder: mpsc::UnboundedSender<oneshot::Sender<Built>>,
}

impl Live {
    pub async fn load(store: Store) -> Result<Arc<Self>, Error> {
        let builder = spawn_builder(&store)?;
        let index = build_on(&builder).await?;
        Ok(Arc::new(Self {
            store,
            index: RwLock::new(Arc::new(index)),
            rebuilding: Mutex::new(()),
            builder,
        }))
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 手上这一份。
    pub fn current(&self) -> Arc<SearchIndex> {
        self.index.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 库里的变更序号和手上这份不一样时重建。返回重建花的时间，没重建时是 `None`。
    pub async fn refresh(&self) -> Result<Option<Duration>, Error> {
        let _one = self.rebuilding.lock().await;
        let latest = self.store.latest_cursor().await?;
        if latest == self.current().corpus().cursor {
            return Ok(None);
        }
        let started = Instant::now();
        let index = build_on(&self.builder).await?;
        // 先换上，旧的在锁外放掉。
        let old = std::mem::replace(
            &mut *self.index.write().unwrap_or_else(|e| e.into_inner()),
            Arc::new(index),
        );
        drop(old);
        Ok(Some(started.elapsed()))
    }

    async fn refresh_logged(&self) {
        match self.refresh().await {
            Ok(Some(took)) => eprintln!(
                "检索索引已更新到变更序号 {}（{} 条，用时 {} 毫秒）",
                self.current().corpus().cursor,
                self.current().corpus().len(),
                took.as_millis()
            ),
            Ok(None) => {}
            Err(e) => eprintln!("更新检索索引失败，继续用旧的：{e}"),
        }
    }

    /// 在后台跟着变更走，直到进程退出。
    pub fn spawn_follow(self: &Arc<Self>) -> JoinHandle<()> {
        let live = self.clone();
        tokio::spawn(async move {
            loop {
                let mut listener = match live.store.listen().await {
                    Ok(l) => Some(l),
                    Err(e) => {
                        eprintln!("订阅变更通知失败，先每 {} 秒查一次：{e}", POLL.as_secs());
                        None
                    }
                };
                // 订阅之前漏掉的变更，在这里补上。
                live.refresh_logged().await;
                let mut tick = tokio::time::interval(POLL);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                tick.tick().await;
                let mut polls = 0;
                loop {
                    tokio::select! {
                        r = next(&mut listener) => if let Err(e) = r {
                            eprintln!("变更通知断了，重新订阅：{e}");
                            break;
                        },
                        _ = tick.tick() => {
                            polls += 1;
                            // 没订阅上时，查两次以后再试着订阅。
                            if listener.is_none() && polls >= 2 {
                                break;
                            }
                        }
                    }
                    live.refresh_logged().await;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
    }
}

async fn next(listener: &mut Option<PgListener>) -> Result<(), String> {
    match listener {
        Some(l) => l.recv().await.map(|_| ()).map_err(|e| e.to_string()),
        None => std::future::pending().await,
    }
}

async fn build_on(builder: &mpsc::UnboundedSender<oneshot::Sender<Built>>) -> Built {
    let (tx, rx) = oneshot::channel();
    builder
        .send(tx)
        .map_err(|_| Error::Join("建索引的线程已经退出".to_owned()))?;
    rx.await
        .map_err(|_| Error::Join("建索引的线程中途退出".to_owned()))?
}

/// 起建索引的线程。`Live` 放掉以后（发送端都没了）线程自己退出。
fn spawn_builder(store: &Store) -> Result<mpsc::UnboundedSender<oneshot::Sender<Built>>, Error> {
    let (tx, mut rx) = mpsc::unbounded_channel::<oneshot::Sender<Built>>();
    let main = store.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Join(format!("建索引线程的运行时：{e}")))?;
    std::thread::Builder::new()
        .name("tcms-index".to_owned())
        .spawn(move || {
            runtime.block_on(async move {
                // 自己的连接，绑在这个运行时上。
                let mut own: Option<Store> = None;
                while let Some(reply) = rx.recv().await {
                    if own.is_none() {
                        match main.sibling(1).await {
                            Ok(s) => own = Some(s),
                            Err(e) => {
                                let _ = reply.send(Err(e.into()));
                                continue;
                            }
                        }
                    }
                    let Some(store) = own.clone() else { continue };
                    // 放进一个任务里跑（单线程运行时，还在这个线程上）：建的时候出了 panic，
                    // 这次报错，线程还在，下次照常建。
                    let built = tokio::spawn(async move { build(&store).await })
                        .await
                        .unwrap_or_else(|e| Err(Error::Join(format!("建索引时出错：{e}"))));
                    let _ = reply.send(built);
                }
            });
        })
        .map_err(|e| Error::Join(format!("起建索引的线程：{e}")))?;
    Ok(tx)
}

async fn build(store: &Store) -> Built {
    let corpus = Corpus::load(store).await?;
    Ok(SearchIndex::build(corpus)?)
}
