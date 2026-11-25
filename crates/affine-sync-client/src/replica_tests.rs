//! 对着内存里的假服务端测本地副本：断网改、两边同时改、新建、重启、广播。真的 AFFiNE 在 `tests/affine.rs`。

use std::collections::{BTreeMap, HashMap};
use std::future::{Future, ready};
use std::sync::Mutex;

use affine_store::Store;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, GetString, ReadTxn, StateVector, Text, Transact, Update};

use super::*;

const WS: &str = "ws1";

#[derive(Default)]
struct State {
    docs: HashMap<String, (Doc, i64)>,
    now: i64,
    offline: bool,
    pushes: Vec<(String, Intent)>,
}

/// 假服务端：每篇文档一个 yrs 的 Doc，时间戳每推一次加一。
#[derive(Default)]
struct Fake(Mutex<State>);

impl Fake {
    fn set_offline(&self, v: bool) {
        self.0.lock().unwrap().offline = v;
    }

    /// 别人在服务端改了一篇（没有这篇就新建）。
    fn edit(&self, doc_id: &str, s: &str) {
        let mut st = self.0.lock().unwrap();
        st.now += 1;
        let now = st.now;
        let entry = st
            .docs
            .entry(doc_id.to_owned())
            .or_insert_with(|| (Doc::new(), 0));
        type_text(&entry.0, s);
        entry.1 = now;
    }

    fn text(&self, doc_id: &str) -> String {
        let st = self.0.lock().unwrap();
        let doc = &st.docs[doc_id].0;
        doc.get_or_insert_text("t").get_string(&doc.transact())
    }

    fn pushes(&self) -> Vec<(String, Intent)> {
        self.0.lock().unwrap().pushes.clone()
    }
}

impl Remote for Fake {
    fn load_doc(
        &self,
        workspace_id: &str,
        doc_id: &str,
        state_vector: Option<&[u8]>,
    ) -> impl Future<Output = Result<Option<LoadedDoc>, Error>> + Send {
        assert_eq!(workspace_id, WS);
        let st = self.0.lock().unwrap();
        let r = if st.offline {
            Err(Error::Disconnected)
        } else {
            Ok(st.docs.get(doc_id).map(|(doc, t)| {
                let sv = state_vector
                    .map(|b| StateVector::decode_v1(b).unwrap())
                    .unwrap_or_default();
                let txn = doc.transact();
                LoadedDoc {
                    update: txn.encode_state_as_update_v1(&sv),
                    state_vector: txn.state_vector().encode_v1(),
                    timestamp: *t,
                }
            }))
        };
        ready(r)
    }

    fn push_update(
        &self,
        workspace_id: &str,
        doc_id: &str,
        update: &[u8],
        intent: Intent,
    ) -> impl Future<Output = Result<Pushed, Error>> + Send {
        assert_eq!(workspace_id, WS);
        let mut st = self.0.lock().unwrap();
        if st.offline {
            return ready(Err(Error::Disconnected));
        }
        st.now += 1;
        let now = st.now;
        st.pushes.push((doc_id.to_owned(), intent));
        let created = !st.docs.contains_key(doc_id);
        let entry = st
            .docs
            .entry(doc_id.to_owned())
            .or_insert_with(|| (Doc::new(), 0));
        entry
            .0
            .transact_mut()
            .apply_update(Update::decode_v1(update).unwrap())
            .unwrap();
        entry.1 = now;
        ready(Ok(Pushed {
            timestamp: now,
            created,
        }))
    }

    fn doc_timestamps(
        &self,
        workspace_id: &str,
        _since: Option<i64>,
    ) -> impl Future<Output = Result<BTreeMap<String, i64>, Error>> + Send {
        assert_eq!(workspace_id, WS);
        let st = self.0.lock().unwrap();
        ready(if st.offline {
            Err(Error::Disconnected)
        } else {
            Ok(st.docs.iter().map(|(k, (_, t))| (k.clone(), *t)).collect())
        })
    }
}

/// 在 `doc` 的文字末尾加一段，返回这次的差量。
fn type_text(doc: &Doc, s: &str) -> Vec<u8> {
    let text = doc.get_or_insert_text("t");
    let before = doc.transact().state_vector();
    {
        let mut txn = doc.transact_mut();
        let len = text.len(&txn);
        text.insert(&mut txn, len, s);
    }
    doc.transact().encode_state_as_update_v1(&before)
}

fn local_text(r: &mut Replica, doc_id: &str) -> String {
    let doc = Doc::new();
    let text = doc.get_or_insert_text("t");
    doc.transact_mut()
        .apply_update(Update::decode_v1(&r.read(doc_id).unwrap().unwrap()).unwrap())
        .unwrap();
    text.get_string(&doc.transact())
}

fn replica() -> Replica {
    Replica::new(Store::open_in_memory().unwrap(), WS, "server").unwrap()
}

#[tokio::test]
async fn offline_edits_are_pushed_when_back_online() {
    let server = Fake::default();
    server.edit("d1", "共同的开头。");
    let mut r = replica();
    let report = r.sync_all(&server).await.unwrap();
    assert_eq!(report.pulled, ["d1"]);
    assert!(report.pushed.is_empty());
    assert_eq!(local_text(&mut r, "d1"), "共同的开头。");
    assert!(r.pending().unwrap().is_empty());

    // 断网：本地改两次，推不上去，记着；服务端那边别人也改了。
    server.set_offline(true);
    let local = Doc::new();
    local
        .transact_mut()
        .apply_update(Update::decode_v1(&r.read("d1").unwrap().unwrap()).unwrap())
        .unwrap();
    r.write("d1", &type_text(&local, "本地断网时写的。"))
        .unwrap();
    let e = r.sync_doc(&server, "d1").await.unwrap_err();
    assert!(e.is_transient(), "{e}");
    r.write("d1", &type_text(&local, "又写一句。")).unwrap();
    assert!(r.sync_all(&server).await.is_err());
    assert_eq!(r.pending().unwrap(), ["d1"]);
    server.set_offline(false);
    server.edit("d1", "别人在服务端写的。");

    // 连上：拉到别人的，推上自己的，两边一样，谁的字都在。
    let report = r.sync_all(&server).await.unwrap();
    assert_eq!(
        (report.pulled, report.pushed),
        (vec!["d1".to_owned()], vec!["d1".to_owned()])
    );
    let mine = local_text(&mut r, "d1");
    assert_eq!(mine, server.text("d1"));
    for part in [
        "共同的开头。",
        "本地断网时写的。",
        "又写一句。",
        "别人在服务端写的。",
    ] {
        assert!(mine.contains(part), "{mine}");
    }
    assert!(r.pending().unwrap().is_empty());
    assert_eq!(server.pushes(), [("d1".to_owned(), Intent::Update)]);

    // 推过以后服务端的时间戳新了，再看一次这篇（差量是空的，不推不拉）；之后什么都不用看。
    let report = r.sync_all(&server).await.unwrap();
    assert_eq!(report.checked, ["d1"]);
    assert!(report.pulled.is_empty() && report.pushed.is_empty());
    assert!(r.sync_all(&server).await.unwrap().checked.is_empty());
    assert_eq!(server.pushes().len(), 1);
}

#[tokio::test]
async fn a_new_local_doc_is_created_on_the_server() {
    let server = Fake::default();
    let mut r = replica();
    r.write("d2", &type_text(&Doc::new(), "新的一篇")).unwrap();
    let report = r.sync_all(&server).await.unwrap();
    assert_eq!(report.pushed, ["d2"]);
    assert_eq!(server.pushes(), [("d2".to_owned(), Intent::Create)]);
    assert_eq!(server.text("d2"), "新的一篇");
    // 推完就没有要推的了。
    assert!(r.pending().unwrap().is_empty());
}

#[tokio::test]
async fn pending_edits_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ws1.db");
    let server = Fake::default();
    server.set_offline(true);
    {
        let mut r = Replica::new(Store::open(&path).unwrap(), WS, "server").unwrap();
        r.write("d1", &type_text(&Doc::new(), "关机前写的"))
            .unwrap();
        assert!(r.sync_doc(&server, "d1").await.is_err());
    }
    server.set_offline(false);
    let mut r = Replica::new(Store::open(&path).unwrap(), WS, "server").unwrap();
    assert_eq!(r.pending().unwrap(), ["d1"]);
    assert_eq!(r.sync_all(&server).await.unwrap().pushed, ["d1"]);
    assert_eq!(server.text("d1"), "关机前写的");

    // 一个库只放一个工作区。
    drop(r);
    assert!(matches!(
        Replica::new(Store::open(&path).unwrap(), "other", "server"),
        Err(Error::Config(_))
    ));
}

#[tokio::test]
async fn broadcasts_go_straight_into_the_store() {
    let server = Fake::default();
    server.edit("d1", "开头");
    let mut r = replica();
    r.sync_all(&server).await.unwrap();

    // 别人改的，广播过来：直接记进库，记下拉到了，不用再拉。
    let theirs = {
        let st = server.0.lock().unwrap();
        let doc = &st.docs["d1"].0;
        let before = doc.transact().state_vector();
        drop(st);
        server.edit("d1", "，广播来的");
        let st = server.0.lock().unwrap();
        st.docs["d1"]
            .0
            .transact()
            .encode_state_as_update_v1(&before)
    };
    let t = server.0.lock().unwrap().docs["d1"].1;
    let ev = SyncEvent::DocUpdates {
        workspace_id: WS.into(),
        doc_id: "d1".into(),
        updates: vec![theirs],
        timestamp: t,
        editor: None,
    };
    assert_eq!(r.apply_event(&ev).unwrap(), FollowUp::Nothing);
    assert_eq!(local_text(&mut r, "d1"), "开头，广播来的");
    let report = r.sync_all(&server).await.unwrap();
    assert!(
        report.pulled.is_empty() && report.pushed.is_empty(),
        "{report:?}"
    );
    assert!(server.pushes().is_empty());

    // 解不开的、重连、工作区有变化：告诉调用的一方接着做什么。别的工作区的不管。
    let bad = SyncEvent::DocUpdates {
        workspace_id: WS.into(),
        doc_id: "d1".into(),
        updates: vec![vec![0xff, 0xff, 0xff]],
        timestamp: t + 1,
        editor: None,
    };
    assert_eq!(r.apply_event(&bad).unwrap(), FollowUp::SyncDoc("d1".into()));
    assert_eq!(
        r.apply_event(&SyncEvent::Reconnected).unwrap(),
        FollowUp::SyncAll
    );
    let inv = SyncEvent::Invalidated {
        workspace_id: WS.into(),
        timestamp: None,
    };
    assert_eq!(r.apply_event(&inv).unwrap(), FollowUp::SyncAll);
    let other = SyncEvent::Invalidated {
        workspace_id: "ws2".into(),
        timestamp: None,
    };
    assert_eq!(r.apply_event(&other).unwrap(), FollowUp::Nothing);
}
