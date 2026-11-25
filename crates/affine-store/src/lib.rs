//! AFFiNE 文档的本地存储（`docs/affine-kernel-rust.md` 的 K4）：一个 SQLite 文件里存一个工作区的
//! 文档更新、合并后的快照、时钟和附件。断网时照样往里写，连上以后按时钟知道哪些还没推上去。
//!
//! 设计参考了 AFFiNE 桌面端 `affine_nbstore`（MIT）的做法，只读了设计，没复制代码，也不依赖它
//! 链接的 AGPL crate：
//!
//! - 每篇文档 = 一份快照 + 后面陆续来的更新（Yjs 的 v1 编码）。读的时候把它们合成一份，顺手写回快照、
//!   删掉合进去的更新（同一个事务里，失败就都不动）。
//! - 每篇文档有一个时钟（毫秒），每来一个更新往前走，保证一篇文档里严格递增。
//! - 对每个同步的对方（`peer`，比如一台 AFFiNE 服务端）记三个时钟：对方那边最新的时钟、已经从对方拉到的时钟、
//!   已经推给对方的本地时钟。本地时钟比“已推”新的文档就是要补推的。
//! - 附件按 `blobId` 存内容、MIME 和大小；删了先标记，可以再彻底删；对每个对方记有没有传上去。
//!
//! 接口是同步的（rusqlite）；在异步代码里用 `spawn_blocking`。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use yrs::updates::decoder::Decode;
use yrs::{Doc, Options, ReadTxn, StateVector, Transact, Update};

/// 表结构的版本，记在 SQLite 的 `user_version` 里。
const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE meta (
  key TEXT PRIMARY KEY NOT NULL,
  value TEXT NOT NULL
);
CREATE TABLE snapshots (
  doc_id TEXT PRIMARY KEY NOT NULL,
  data BLOB NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE updates (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  doc_id TEXT NOT NULL,
  data BLOB NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX updates_doc_id ON updates (doc_id, seq);
CREATE TABLE clocks (
  doc_id TEXT PRIMARY KEY NOT NULL,
  clock INTEGER NOT NULL
);
CREATE TABLE peer_clocks (
  peer TEXT NOT NULL,
  doc_id TEXT NOT NULL,
  remote_clock INTEGER NOT NULL DEFAULT 0,
  pulled_remote_clock INTEGER NOT NULL DEFAULT 0,
  pushed_clock INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (peer, doc_id)
);
CREATE TABLE blobs (
  key TEXT PRIMARY KEY NOT NULL,
  data BLOB NOT NULL,
  mime TEXT NOT NULL,
  size INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  deleted_at INTEGER
);
CREATE TABLE peer_blobs (
  peer TEXT NOT NULL,
  blob_id TEXT NOT NULL,
  uploaded_at INTEGER NOT NULL,
  PRIMARY KEY (peer, blob_id)
);
"#;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("SQLite 出错：{0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("不是合法的 Yjs 更新：{0}")]
    BadUpdate(String),
    #[error("本地库的表结构版本是 {0}，这个版本只认得 {SCHEMA_VERSION}")]
    SchemaVersion(i64),
}

pub type Result<T> = std::result::Result<T, Error>;

/// 一篇文档合成以后的样子。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocRecord {
    pub doc_id: String,
    /// 整篇的 Yjs 更新（v1）。
    pub bin: Vec<u8>,
    /// 这篇文档的时钟（毫秒）。
    pub clock: i64,
}

/// 一篇文档的时钟。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocClock {
    pub doc_id: String,
    pub clock: i64,
}

/// 一个附件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub key: String,
    pub data: Vec<u8>,
    pub mime: String,
    pub created_at: i64,
}

/// 附件的信息（不带内容）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobMeta {
    pub key: String,
    pub mime: String,
    pub size: i64,
    pub created_at: i64,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn check_update(bin: &[u8]) -> Result<()> {
    Update::decode_v1(bin)
        .map(|_| ())
        .map_err(|e| Error::BadUpdate(e.to_string()))
}

/// 把几条更新合成一份。
///
/// 放进一个空的 Doc 里应用，再整篇编码出来：比 yrs 的 `merge_updates_v1` 快得多（1 万条小更新约 7 毫秒对约 500 毫秒，
/// `tcms bench affine-doc`），结果也小（连着打的字合成一段）。不做垃圾回收（`skip_gc`），删掉的内容和 `merge_updates_v1` 一样留着。
/// 有缺了前面的更新、放不进去的（比如先收到后一条），退回 `merge_updates_v1`：它把放不进去的也原样留在结果里。
pub fn merge_updates(updates: &[&[u8]]) -> Result<Vec<u8>> {
    let doc = Doc::with_options(Options {
        skip_gc: true,
        ..Options::default()
    });
    // 一条一个事务：全放进一个事务里反而慢（1 万条约 90 毫秒，一条一个约 7 毫秒）。
    for u in updates {
        let u = Update::decode_v1(u).map_err(|e| Error::BadUpdate(e.to_string()))?;
        doc.transact_mut()
            .apply_update(u)
            .map_err(|e| Error::BadUpdate(e.to_string()))?;
    }
    let complete = !doc.transact().has_missing_updates();
    if complete {
        Ok(doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default()))
    } else {
        yrs::merge_updates_v1(updates.to_vec()).map_err(|e| Error::BadUpdate(e.to_string()))
    }
}

/// 空的更新（什么都没改）：两个字节 0，0。
fn is_empty_update(bin: &[u8]) -> bool {
    bin.is_empty() || bin == [0, 0]
}

/// 一个工作区的本地库。
pub struct Store {
    conn: Connection,
    now: Box<dyn Fn() -> i64 + Send>,
}

impl Store {
    /// 打开（没有就新建）一个本地库文件。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    /// 只在内存里的库，测试和临时用。
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // WAL：读写不互相挡；NORMAL：WAL 下断电最多丢最后几个事务，不会坏库。
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => {
                conn.execute_batch(&format!(
                    "BEGIN; {SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
                ))?;
            }
            SCHEMA_VERSION => {}
            other => return Err(Error::SchemaVersion(other)),
        }
        Ok(Self {
            conn,
            now: Box::new(now_ms),
        })
    }

    /// 换掉取当前时间的函数（测试用）。
    pub fn set_clock_source(&mut self, now: impl Fn() -> i64 + Send + 'static) {
        self.now = Box::new(now);
    }

    // ------------------------------------------------------------ 工作区

    /// 这个库属于哪个工作区。
    pub fn space_id(&self) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'space_id'", [], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_space_id(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES ('space_id', ?1)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [id],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ 文档

    /// 记下一个更新（本地改的，或者从对方拉来的），返回这篇文档新的时钟。空更新不记，返回现在的时钟。
    pub fn push_update(&mut self, doc_id: &str, update: &[u8]) -> Result<i64> {
        check_update(update)?;
        let now = (self.now)();
        let tx = self.conn.transaction()?;
        let last: Option<i64> = tx
            .query_row(
                "SELECT clock FROM clocks WHERE doc_id = ?1",
                [doc_id],
                |r| r.get(0),
            )
            .optional()?;
        if is_empty_update(update) {
            tx.commit()?;
            return Ok(last.unwrap_or(0));
        }
        // 一篇文档里时钟严格递增：同一毫秒、或者系统时间往回拨了，就在上一个时钟上加一。
        let clock = match last {
            Some(l) if now <= l => l + 1,
            _ => now,
        };
        tx.execute(
            "INSERT INTO updates (doc_id, data, created_at) VALUES (?1, ?2, ?3)",
            params![doc_id, update, clock],
        )?;
        tx.execute(
            "INSERT INTO clocks (doc_id, clock) VALUES (?1, ?2)
             ON CONFLICT (doc_id) DO UPDATE SET clock = excluded.clock",
            params![doc_id, clock],
        )?;
        tx.commit()?;
        Ok(clock)
    }

    /// 读一篇文档：快照和后面的更新合成一份。合了更新的话写回快照、删掉合进去的更新。
    pub fn get_doc(&mut self, doc_id: &str) -> Result<Option<DocRecord>> {
        let tx = self.conn.transaction()?;
        let snapshot: Option<Vec<u8>> = tx
            .query_row(
                "SELECT data FROM snapshots WHERE doc_id = ?1",
                [doc_id],
                |r| r.get(0),
            )
            .optional()?;
        let updates: Vec<(i64, Vec<u8>)> = {
            let mut st =
                tx.prepare("SELECT seq, data FROM updates WHERE doc_id = ?1 ORDER BY seq")?;
            st.query_map([doc_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let clock: i64 = tx
            .query_row(
                "SELECT clock FROM clocks WHERE doc_id = ?1",
                [doc_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if snapshot.is_none() && updates.is_empty() {
            return Ok(None);
        }
        let bin = if updates.is_empty() {
            snapshot.unwrap_or_default()
        } else {
            let mut all: Vec<&[u8]> = Vec::with_capacity(updates.len() + 1);
            if let Some(s) = snapshot.as_deref() {
                all.push(s);
            }
            all.extend(updates.iter().map(|(_, d)| d.as_slice()));
            let merged = merge_updates(&all)?;
            tx.execute(
                "INSERT INTO snapshots (doc_id, data, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (doc_id) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at",
                params![doc_id, merged, clock],
            )?;
            let last_seq = updates.last().map(|(s, _)| *s).unwrap_or_default();
            tx.execute(
                "DELETE FROM updates WHERE doc_id = ?1 AND seq <= ?2",
                params![doc_id, last_seq],
            )?;
            merged
        };
        tx.commit()?;
        Ok(Some(DocRecord {
            doc_id: doc_id.to_owned(),
            bin,
            clock,
        }))
    }

    /// 对方缺的那部分：给对方的状态向量（v1），返回差量和本地的状态向量。没有这篇文档时 `None`。
    pub fn get_doc_diff(
        &mut self,
        doc_id: &str,
        state_vector: Option<&[u8]>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let Some(doc) = self.get_doc(doc_id)? else {
            return Ok(None);
        };
        let sv = match state_vector {
            Some(sv) => sv.to_vec(),
            None => {
                use yrs::updates::encoder::Encode;
                StateVector::default().encode_v1()
            }
        };
        let diff =
            yrs::diff_updates_v1(&doc.bin, &sv).map_err(|e| Error::BadUpdate(e.to_string()))?;
        let local = yrs::encode_state_vector_from_update_v1(&doc.bin)
            .map_err(|e| Error::BadUpdate(e.to_string()))?;
        Ok(Some((diff, local)))
    }

    /// 整篇替换快照（比如从对方拉来一整篇）。只有比本地时钟新时才写，返回写没写。
    /// 之前的更新已经包含在对方的整篇里时才该用它；拿不准就用 [`Store::push_update`]。
    pub fn set_doc_snapshot(&mut self, doc_id: &str, bin: &[u8], clock: i64) -> Result<bool> {
        check_update(bin)?;
        let tx = self.conn.transaction()?;
        let last: Option<i64> = tx
            .query_row(
                "SELECT clock FROM clocks WHERE doc_id = ?1",
                [doc_id],
                |r| r.get(0),
            )
            .optional()?;
        if last.is_some_and(|l| l >= clock) {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO snapshots (doc_id, data, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (doc_id) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at",
            params![doc_id, bin, clock],
        )?;
        tx.execute("DELETE FROM updates WHERE doc_id = ?1", [doc_id])?;
        tx.execute(
            "INSERT INTO clocks (doc_id, clock) VALUES (?1, ?2)
             ON CONFLICT (doc_id) DO UPDATE SET clock = excluded.clock",
            params![doc_id, clock],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// 还没合进快照的更新有几个（看要不要合）。
    pub fn pending_updates(&self, doc_id: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM updates WHERE doc_id = ?1",
            [doc_id],
            |r| r.get(0),
        )?)
    }

    /// 删掉一篇文档的全部记录（快照、更新、时钟、各对方的时钟）。
    pub fn delete_doc(&mut self, doc_id: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        for table in ["snapshots", "updates", "clocks", "peer_clocks"] {
            tx.execute(&format!("DELETE FROM {table} WHERE doc_id = ?1"), [doc_id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// 全部文档的时钟；给了 `after` 时只要比它新的。按文档 id 排。
    pub fn doc_clocks(&self, after: Option<i64>) -> Result<Vec<DocClock>> {
        let mut st = self
            .conn
            .prepare("SELECT doc_id, clock FROM clocks WHERE clock > ?1 ORDER BY doc_id")?;
        let rows = st
            .query_map([after.unwrap_or(i64::MIN)], |r| {
                Ok(DocClock {
                    doc_id: r.get(0)?,
                    clock: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    // ------------------------------------------------------------ 和对方同步的时钟

    fn peer_clock(&self, column: &str, peer: &str, doc_id: &str) -> Result<i64> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {column} FROM peer_clocks WHERE peer = ?1 AND doc_id = ?2"),
                [peer, doc_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// 只往前走：给的比记着的旧就不动。
    fn set_peer_clock(&self, column: &str, peer: &str, doc_id: &str, clock: i64) -> Result<()> {
        self.conn.execute(
            &format!(
                "INSERT INTO peer_clocks (peer, doc_id, {column}) VALUES (?1, ?2, ?3)
                 ON CONFLICT (peer, doc_id) DO UPDATE SET {column} = max({column}, excluded.{column})"
            ),
            params![peer, doc_id, clock],
        )?;
        Ok(())
    }

    /// 对方那边这篇文档最新的时钟（对方告诉我们的）。
    pub fn remote_clock(&self, peer: &str, doc_id: &str) -> Result<i64> {
        self.peer_clock("remote_clock", peer, doc_id)
    }

    pub fn set_remote_clock(&self, peer: &str, doc_id: &str, clock: i64) -> Result<()> {
        self.set_peer_clock("remote_clock", peer, doc_id, clock)
    }

    /// 已经从对方拉到哪个时钟。
    pub fn pulled_remote_clock(&self, peer: &str, doc_id: &str) -> Result<i64> {
        self.peer_clock("pulled_remote_clock", peer, doc_id)
    }

    pub fn set_pulled_remote_clock(&self, peer: &str, doc_id: &str, clock: i64) -> Result<()> {
        self.set_peer_clock("pulled_remote_clock", peer, doc_id, clock)
    }

    /// 本地的改动推给对方推到了哪个时钟。
    pub fn pushed_clock(&self, peer: &str, doc_id: &str) -> Result<i64> {
        self.peer_clock("pushed_clock", peer, doc_id)
    }

    pub fn set_pushed_clock(&self, peer: &str, doc_id: &str, clock: i64) -> Result<()> {
        self.set_peer_clock("pushed_clock", peer, doc_id, clock)
    }

    /// 要推给对方的文档：本地时钟比推过的新。按文档 id 排。
    pub fn docs_to_push(&self, peer: &str) -> Result<Vec<DocClock>> {
        let mut st = self.conn.prepare(
            "SELECT c.doc_id, c.clock FROM clocks c
             LEFT JOIN peer_clocks p ON p.peer = ?1 AND p.doc_id = c.doc_id
             WHERE c.clock > coalesce(p.pushed_clock, 0)
             ORDER BY c.doc_id",
        )?;
        let rows = st
            .query_map([peer], |r| {
                Ok(DocClock {
                    doc_id: r.get(0)?,
                    clock: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// 要从对方拉的文档：对方的时钟比拉到的新。按文档 id 排，时钟是对方的。
    pub fn docs_to_pull(&self, peer: &str) -> Result<Vec<DocClock>> {
        let mut st = self.conn.prepare(
            "SELECT doc_id, remote_clock FROM peer_clocks
             WHERE peer = ?1 AND remote_clock > pulled_remote_clock
             ORDER BY doc_id",
        )?;
        let rows = st
            .query_map([peer], |r| {
                Ok(DocClock {
                    doc_id: r.get(0)?,
                    clock: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    // ------------------------------------------------------------ 附件

    /// 存一个附件。同一个 `key` 再存就覆盖，删除标记去掉。
    pub fn set_blob(&self, key: &str, data: &[u8], mime: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO blobs (key, data, mime, size, created_at, deleted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL)
             ON CONFLICT (key) DO UPDATE SET data = excluded.data, mime = excluded.mime,
               size = excluded.size, deleted_at = NULL",
            params![key, data, mime, data.len() as i64, (self.now)()],
        )?;
        Ok(())
    }

    /// 取一个附件；标了删除的取不到。
    pub fn get_blob(&self, key: &str) -> Result<Option<Blob>> {
        Ok(self
            .conn
            .query_row(
                "SELECT key, data, mime, created_at FROM blobs WHERE key = ?1 AND deleted_at IS NULL",
                [key],
                |r| {
                    Ok(Blob {
                        key: r.get(0)?,
                        data: r.get(1)?,
                        mime: r.get(2)?,
                        created_at: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// 删附件：`permanently` 为假时只标记（还能再存回来），为真时连内容一起删掉。
    pub fn delete_blob(&self, key: &str, permanently: bool) -> Result<()> {
        if permanently {
            self.conn
                .execute("DELETE FROM blobs WHERE key = ?1", [key])?;
            self.conn
                .execute("DELETE FROM peer_blobs WHERE blob_id = ?1", [key])?;
        } else {
            self.conn.execute(
                "UPDATE blobs SET deleted_at = ?2 WHERE key = ?1 AND deleted_at IS NULL",
                params![key, (self.now)()],
            )?;
        }
        Ok(())
    }

    /// 没标删除的附件，按 `key` 排。
    pub fn list_blobs(&self) -> Result<Vec<BlobMeta>> {
        let mut st = self.conn.prepare(
            "SELECT key, mime, size, created_at FROM blobs WHERE deleted_at IS NULL ORDER BY key",
        )?;
        let rows = st
            .query_map([], |r| {
                Ok(BlobMeta {
                    key: r.get(0)?,
                    mime: r.get(1)?,
                    size: r.get(2)?,
                    created_at: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// 记下附件已经传给了对方。
    pub fn set_blob_uploaded(&self, peer: &str, key: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO peer_blobs (peer, blob_id, uploaded_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (peer, blob_id) DO UPDATE SET uploaded_at = excluded.uploaded_at",
            params![peer, key, (self.now)()],
        )?;
        Ok(())
    }

    /// 还没传给对方的附件 `key`，按 `key` 排。
    pub fn blobs_to_upload(&self, peer: &str) -> Result<Vec<String>> {
        let mut st = self.conn.prepare(
            "SELECT b.key FROM blobs b
             LEFT JOIN peer_blobs p ON p.peer = ?1 AND p.blob_id = b.key
             WHERE b.deleted_at IS NULL AND p.blob_id IS NULL
             ORDER BY b.key",
        )?;
        let rows = st
            .query_map([peer], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
}
