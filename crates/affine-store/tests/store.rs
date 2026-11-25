use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use affine_store::{DocClock, Error, Store};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, GetString, ReadTxn, StateVector, Text, Transact, Update};

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

fn content(bin: &[u8]) -> String {
    let doc = Doc::new();
    let text = doc.get_or_insert_text("t");
    doc.transact_mut()
        .apply_update(Update::decode_v1(bin).unwrap())
        .unwrap();
    text.get_string(&doc.transact())
}

/// 固定的时钟，测试里手动往前拨。
fn fixed_clock(store: &mut Store, start: i64) -> Arc<AtomicI64> {
    let t = Arc::new(AtomicI64::new(start));
    let c = t.clone();
    store.set_clock_source(move || c.load(Ordering::SeqCst));
    t
}

#[test]
fn updates_merge_into_a_snapshot() {
    let mut store = Store::open_in_memory().unwrap();
    let doc = Doc::new();
    store.push_update("d1", &type_text(&doc, "你好")).unwrap();
    store.push_update("d1", &type_text(&doc, "，世界")).unwrap();
    assert_eq!(store.pending_updates("d1").unwrap(), 2);
    let got = store.get_doc("d1").unwrap().unwrap();
    assert_eq!(content(&got.bin), "你好，世界");
    // 读过一次就合进快照了，再读一样。
    assert_eq!(store.pending_updates("d1").unwrap(), 0);
    assert_eq!(store.get_doc("d1").unwrap().unwrap(), got);
    store.push_update("d1", &type_text(&doc, "！")).unwrap();
    assert_eq!(
        content(&store.get_doc("d1").unwrap().unwrap().bin),
        "你好，世界！"
    );
    assert!(store.get_doc("nope").unwrap().is_none());
}

#[test]
fn survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ws.db");
    let doc = Doc::new();
    {
        let mut store = Store::open(&path).unwrap();
        store.set_space_id("ws1").unwrap();
        store.push_update("d1", &type_text(&doc, "重启前")).unwrap();
        store.set_blob("b1", b"png", "image/png").unwrap();
    }
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.space_id().unwrap().as_deref(), Some("ws1"));
    assert_eq!(
        content(&store.get_doc("d1").unwrap().unwrap().bin),
        "重启前"
    );
    assert_eq!(store.get_blob("b1").unwrap().unwrap().data, b"png");
}

#[test]
fn offline_edits_are_pushed_later_and_concurrent_edits_merge() {
    let mut store = Store::open_in_memory().unwrap();
    let t = fixed_clock(&mut store, 1_000);
    // 本地和服务端各一份，先同步到一样。
    let local = Doc::new();
    let first = type_text(&local, "共同的开头。");
    store.push_update("d1", &first).unwrap();
    store.set_pushed_clock("server", "d1", 1_000).unwrap();
    let server = Doc::new();
    server
        .transact_mut()
        .apply_update(Update::decode_v1(&first).unwrap())
        .unwrap();
    assert!(store.docs_to_push("server").unwrap().is_empty());

    // 断网：本地改了两次，服务端那边别人也改了。
    t.store(2_000, Ordering::SeqCst);
    store
        .push_update("d1", &type_text(&local, "本地断网时写的。"))
        .unwrap();
    t.store(3_000, Ordering::SeqCst);
    let clock = store
        .push_update("d1", &type_text(&local, "又写一句。"))
        .unwrap();
    let theirs = type_text(&server, "别人在服务端写的。");
    assert_eq!(
        store.docs_to_push("server").unwrap(),
        [DocClock {
            doc_id: "d1".into(),
            clock
        }]
    );

    // 连上：按服务端的状态向量取差量推过去，再把服务端的改动拉下来。
    let server_sv = server.transact().state_vector().encode_v1();
    let (diff, _) = store.get_doc_diff("d1", Some(&server_sv)).unwrap().unwrap();
    server
        .transact_mut()
        .apply_update(Update::decode_v1(&diff).unwrap())
        .unwrap();
    store.set_pushed_clock("server", "d1", clock).unwrap();
    t.store(4_000, Ordering::SeqCst);
    store.set_remote_clock("server", "d1", 9_000).unwrap();
    assert_eq!(store.docs_to_pull("server").unwrap().len(), 1);
    let pulled_at = store.push_update("d1", &theirs).unwrap();
    store
        .set_pulled_remote_clock("server", "d1", 9_000)
        .unwrap();
    assert!(store.docs_to_pull("server").unwrap().is_empty());

    // 两边一样，谁的字都在。拉来的改动也让本地时钟往前走，所以这篇又列在要推的里；
    // 推的时候按服务端的状态向量取差量，是空的，不会把别人的改动再推回去。
    let mine = content(&store.get_doc("d1").unwrap().unwrap().bin);
    let on_server = server
        .get_or_insert_text("t")
        .get_string(&server.transact());
    assert_eq!(mine, on_server);
    for part in [
        "共同的开头。",
        "本地断网时写的。",
        "又写一句。",
        "别人在服务端写的。",
    ] {
        assert!(mine.contains(part), "{mine}");
    }
    assert_eq!(
        store.docs_to_push("server").unwrap(),
        [DocClock {
            doc_id: "d1".into(),
            clock: pulled_at
        }]
    );
    // 状态向量一样时，差量是空的。
    let sv = server.transact().state_vector().encode_v1();
    let (diff, _) = store.get_doc_diff("d1", Some(&sv)).unwrap().unwrap();
    let before = server.transact().state_vector();
    server
        .transact_mut()
        .apply_update(Update::decode_v1(&diff).unwrap())
        .unwrap();
    assert_eq!(server.transact().state_vector(), before);
}

#[test]
fn clocks_only_go_forward() {
    let mut store = Store::open_in_memory().unwrap();
    let t = fixed_clock(&mut store, 5_000);
    let doc = Doc::new();
    assert_eq!(
        store.push_update("d1", &type_text(&doc, "a")).unwrap(),
        5_000
    );
    // 同一毫秒、时间往回拨，都在上一个上加一。
    assert_eq!(
        store.push_update("d1", &type_text(&doc, "b")).unwrap(),
        5_001
    );
    t.store(4_000, Ordering::SeqCst);
    assert_eq!(
        store.push_update("d1", &type_text(&doc, "c")).unwrap(),
        5_002
    );
    t.store(6_000, Ordering::SeqCst);
    assert_eq!(
        store
            .push_update("d2", &type_text(&Doc::new(), "x"))
            .unwrap(),
        6_000
    );
    // 空更新不记。
    assert_eq!(store.push_update("d1", &[0, 0]).unwrap(), 5_002);
    assert_eq!(store.pending_updates("d1").unwrap(), 3);
    let ids = |v: Vec<DocClock>| v.into_iter().map(|c| c.doc_id).collect::<Vec<_>>();
    assert_eq!(ids(store.doc_clocks(None).unwrap()), ["d1", "d2"]);
    assert_eq!(ids(store.doc_clocks(Some(5_002)).unwrap()), ["d2"]);
    // 对方的时钟也只往前走。
    store.set_pushed_clock("p", "d1", 5_002).unwrap();
    store.set_pushed_clock("p", "d1", 10).unwrap();
    assert_eq!(store.pushed_clock("p", "d1").unwrap(), 5_002);
    assert_eq!(store.pushed_clock("other", "d1").unwrap(), 0);
}

#[test]
fn whole_snapshot_only_when_newer() {
    let mut store = Store::open_in_memory().unwrap();
    fixed_clock(&mut store, 100);
    let doc = Doc::new();
    store.push_update("d1", &type_text(&doc, "旧的")).unwrap();
    let newer = Doc::new();
    type_text(&newer, "整篇换掉");
    let full = newer
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    assert!(!store.set_doc_snapshot("d1", &full, 50).unwrap());
    assert_eq!(content(&store.get_doc("d1").unwrap().unwrap().bin), "旧的");
    assert!(store.set_doc_snapshot("d1", &full, 200).unwrap());
    let got = store.get_doc("d1").unwrap().unwrap();
    assert_eq!((content(&got.bin).as_str(), got.clock), ("整篇换掉", 200));
}

#[test]
fn bad_updates_are_rejected() {
    let mut store = Store::open_in_memory().unwrap();
    assert!(matches!(
        store.push_update("d1", b"\xff\xff\xff"),
        Err(Error::BadUpdate(_))
    ));
    assert!(store.get_doc("d1").unwrap().is_none());
    assert!(store.doc_clocks(None).unwrap().is_empty());
}

#[test]
fn blobs_and_uploads() {
    let store = Store::open_in_memory().unwrap();
    store.set_blob("b2", b"two", "text/plain").unwrap();
    store.set_blob("b1", b"one", "image/png").unwrap();
    let keys = |s: &Store| {
        s.list_blobs()
            .unwrap()
            .into_iter()
            .map(|b| b.key)
            .collect::<Vec<_>>()
    };
    assert_eq!(keys(&store), ["b1", "b2"]);
    assert_eq!(store.list_blobs().unwrap()[0].size, 3);
    assert_eq!(store.blobs_to_upload("server").unwrap(), ["b1", "b2"]);
    store.set_blob_uploaded("server", "b1").unwrap();
    assert_eq!(store.blobs_to_upload("server").unwrap(), ["b2"]);
    // 标了删除：取不到、不列、不传；再存回来又有了。
    store.delete_blob("b2", false).unwrap();
    assert!(store.get_blob("b2").unwrap().is_none());
    assert_eq!(keys(&store), ["b1"]);
    assert!(store.blobs_to_upload("server").unwrap().is_empty());
    store.set_blob("b2", b"two again", "text/plain").unwrap();
    assert_eq!(store.get_blob("b2").unwrap().unwrap().data, b"two again");
    // 彻底删：连上传记录一起没了。
    store.delete_blob("b1", true).unwrap();
    store.set_blob("b1", b"new", "image/png").unwrap();
    assert_eq!(store.blobs_to_upload("server").unwrap(), ["b1", "b2"]);
}

#[test]
fn deleting_a_doc_removes_everything() {
    let mut store = Store::open_in_memory().unwrap();
    let doc = Doc::new();
    store.push_update("d1", &type_text(&doc, "a")).unwrap();
    store.get_doc("d1").unwrap();
    store.push_update("d1", &type_text(&doc, "b")).unwrap();
    store.set_pushed_clock("p", "d1", 1).unwrap();
    store.delete_doc("d1").unwrap();
    assert!(store.get_doc("d1").unwrap().is_none());
    assert!(store.doc_clocks(None).unwrap().is_empty());
    assert_eq!(store.pushed_clock("p", "d1").unwrap(), 0);
}

#[test]
fn refuses_a_newer_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ws.db");
    drop(Store::open(&path).unwrap());
    set_user_version(&path, 99);
    assert!(matches!(Store::open(&path), Err(Error::SchemaVersion(99))));
}

/// 不想让测试多依赖 rusqlite，直接改文件头里的 user_version（偏移 60，大端 4 字节）。
fn set_user_version(path: &std::path::Path, v: u32) {
    let mut bytes = std::fs::read(path).unwrap();
    bytes[60..64].copy_from_slice(&v.to_be_bytes());
    std::fs::write(path, bytes).unwrap();
}
