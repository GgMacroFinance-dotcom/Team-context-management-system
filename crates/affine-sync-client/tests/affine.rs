//! 对着真的 AFFiNE 测（自托管，0.27.5 起）。要设：
//!
//! - `AFFINE_TEST_URL`（比如 `http://127.0.0.1:3010`）
//! - `AFFINE_TEST_EMAIL`、`AFFINE_TEST_PASSWORD`：一个能建工作区的账号
//!
//! 没设就跳过（CI 里没有 AFFiNE）。每跑一次新建一个工作区，不删。

use std::time::Duration;

use affine_doc::{RandomIds, Section, workspace};
use affine_sync_client::{
    Client, Config, Credentials, DocMode, Error, FollowUp, Intent, Replica, SyncConnection,
    SyncEvent, comment_content, comment_text,
};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, ReadTxn, StateVector, Transact, Update};

fn config() -> Option<Config> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let (url, email, password) = (
        var("AFFINE_TEST_URL")?,
        var("AFFINE_TEST_EMAIL")?,
        var("AFFINE_TEST_PASSWORD")?,
    );
    Some(Config {
        url,
        credentials: Credentials { email, password },
        client_version: None,
        extra_ca: None,
    })
}

fn apply(doc: &Doc, update: &[u8]) {
    doc.transact_mut()
        .apply_update(Update::decode_v1(update).unwrap())
        .unwrap();
}

fn markdown(doc: &Doc) -> String {
    affine_doc::to_markdown(&affine_doc::read(doc).unwrap().blocks)
}

/// 工作区根文档的 `meta.pages` 里加一篇，返回这次的差量。
fn add_page(root: &Doc, id: &str, title: &str) -> Vec<u8> {
    let sv = root.transact().state_vector();
    workspace::upsert_doc_meta(root, &workspace::DocMeta::new(id, title, 1.7e12));
    root.transact().encode_state_as_update_v1(&sv)
}

/// 等下一次某篇文档的修改，别的事件跳过。
async fn next_update(conn: &mut SyncConnection, doc_id: &str) -> (Vec<Vec<u8>>, Option<String>) {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(20), conn.next_event())
            .await
            .expect("等修改超时")
            .expect("连接没了");
        if let SyncEvent::DocUpdates {
            doc_id: d,
            updates,
            editor,
            ..
        } = ev
            && d == doc_id
        {
            return (updates, editor);
        }
    }
}

#[tokio::test]
async fn sync_with_a_real_affine() {
    let Some(config) = config() else {
        eprintln!("没设 AFFINE_TEST_URL、AFFINE_TEST_EMAIL、AFFINE_TEST_PASSWORD，跳过");
        return;
    };
    let client = Client::new(config.clone()).unwrap();
    let info = client.server_info().await.unwrap();
    assert!(
        info.version.starts_with("0.27.5") || info.version.as_str() > "0.27.5",
        "{info:?}"
    );
    let user = client.user_id().await.unwrap();

    // 新工作区：先推根文档，再推一篇文档，再把它记进根文档的文档列表
    let ws = client.create_workspace().await.unwrap();
    let mut a = client.sync().await.unwrap();
    let root = Doc::new();
    let first = add_page(&root, "tcmsTest01", "测试文档");
    assert!(
        a.push_update(&ws, &ws, &first, Intent::Create)
            .await
            .unwrap()
            .timestamp
            > 0
    );
    let page = Doc::new();
    let md = "## 第一条\n\n正文 **粗体** 和 `代码`\n\n- 列表\n  - 下一级\n\n## 第二条\n\n1. 编号\n";
    affine_doc::create(
        &page,
        "测试文档",
        &affine_doc::to_blocks(md),
        &mut RandomIds,
    )
    .unwrap();
    let doc_id = "tcmsTest01";
    let full = page
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    let pushed = a
        .push_update(&ws, doc_id, &full, Intent::Create)
        .await
        .unwrap();
    assert!(pushed.created);

    // 读回来一样
    let loaded = a.load_doc(&ws, doc_id, None).await.unwrap().unwrap();
    let back = Doc::new();
    apply(&back, &loaded.update);
    assert_eq!(markdown(&back), md.trim_end());
    assert_eq!(affine_doc::read(&back).unwrap().title, "测试文档");
    let loaded_root = a.load_doc(&ws, &ws, None).await.unwrap().unwrap();
    let root_back = Doc::new();
    apply(&root_back, &loaded_root.update);
    let pages = workspace::read_meta(&root_back).pages;
    assert!(
        pages
            .iter()
            .any(|p| p.id == "tcmsTest01" && p.title == "测试文档"),
        "{pages:?}"
    );
    assert!(a.load_doc(&ws, "noSuchDoc", None).await.unwrap().is_none());

    // 另一条连接收得到这条连接推的修改
    let mut b = client.sync().await.unwrap();
    let b_doc = Doc::new();
    apply(
        &b_doc,
        &b.load_doc(&ws, doc_id, None).await.unwrap().unwrap().update,
    );
    let item = |doc: &Doc, n: usize| {
        affine_doc::split(&affine_doc::read(doc).unwrap().blocks)
            .into_iter()
            .filter_map(|s| match s {
                Section::Item(i) => Some(i),
                Section::Loose(_) => None,
            })
            .nth(n)
            .unwrap()
    };
    let sv = page.transact().state_vector();
    let it = item(&page, 1);
    let body = affine_doc::to_blocks("1. 编号\n\nA 加的一段");
    affine_doc::update_item(
        &page,
        it.heading_id.as_deref().unwrap(),
        &it.title,
        &body,
        &mut RandomIds,
    )
    .unwrap();
    let delta = page.transact().encode_state_as_update_v1(&sv);
    a.push_update(&ws, doc_id, &delta, Intent::Update)
        .await
        .unwrap();
    let (updates, editor) = next_update(&mut b, doc_id).await;
    assert_eq!(editor.as_deref(), Some(user.as_str()));
    for u in &updates {
        apply(&b_doc, u);
    }
    assert!(markdown(&b_doc).contains("A 加的一段"));
    assert_eq!(markdown(&b_doc), markdown(&page));

    // 哪些文档变过
    let ts = a.doc_timestamps(&ws, None).await.unwrap();
    assert!(ts.contains_key(doc_id) && ts.contains_key(&ws), "{ts:?}");
    let later = a.doc_timestamps(&ws, Some(i64::MAX - 1)).await.unwrap();
    assert!(later.is_empty(), "{later:?}");

    // 断开重连：重新加入以后还收得到别人改的
    a.reconnect().await;
    let mut saw_disconnect = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(30), a.next_event())
            .await
            .unwrap()
            .unwrap()
        {
            SyncEvent::Disconnected { .. } => saw_disconnect = true,
            SyncEvent::Reconnected => break,
            _ => {}
        }
    }
    assert!(saw_disconnect);
    let sv = b_doc.transact().state_vector();
    let it = item(&b_doc, 0);
    let body = affine_doc::to_blocks("正文 **粗体** 和 `代码`\n\nB 在重连以后加的");
    affine_doc::update_item(
        &b_doc,
        it.heading_id.as_deref().unwrap(),
        &it.title,
        &body,
        &mut RandomIds,
    )
    .unwrap();
    let delta = b_doc.transact().encode_state_as_update_v1(&sv);
    b.push_update(&ws, doc_id, &delta, Intent::Update)
        .await
        .unwrap();
    let (updates, _) = next_update(&mut a, doc_id).await;
    for u in &updates {
        apply(&page, u);
    }
    assert!(markdown(&page).contains("B 在重连以后加的"));

    // 拿状态向量取差量：跟上以后什么都不缺
    let sv = page.transact().state_vector().encode_v1();
    let diff = a.load_doc(&ws, doc_id, Some(&sv)).await.unwrap().unwrap();
    apply(&page, &diff.update);
    assert_eq!(markdown(&page), markdown(&b_doc));
    let before = page.transact().state_vector();
    apply(&page, &diff.update);
    assert_eq!(page.transact().state_vector(), before);

    // GraphQL：工作区、最近改过的文档、评论
    assert!(
        client
            .workspaces()
            .await
            .unwrap()
            .iter()
            .any(|w| w.id == ws)
    );
    let recent = client.recently_updated_docs(&ws, 20, None).await.unwrap();
    assert!(
        recent.items.iter().any(|d| d.id == doc_id),
        "{:?}",
        recent.items
    );
    let content = comment_content(&["提议：把第二条改成这样", "理由：……"], Some("编号")).unwrap();
    let c = client
        .create_comment(&ws, doc_id, "测试文档", DocMode::Page, content)
        .await
        .unwrap();
    let listed = client.comments(&ws, doc_id).await.unwrap();
    // 一次问几篇：超过一次查询的篇数（分两次问），没有的文档是空的。
    let mut many: Vec<String> = (0..affine_sync_client::DOCS_PER_QUERY)
        .map(|i| format!("no-such-doc-{i}"))
        .collect();
    many.push(doc_id.to_owned());
    let of = client.comments_of(&ws, &many).await.unwrap();
    assert_eq!(of.len(), many.len());
    assert_eq!(of[doc_id], listed);
    assert!(of["no-such-doc-0"].is_empty());
    let found = listed.iter().find(|x| x.id == c.id).unwrap();
    assert_eq!(
        comment_text(&found.content),
        "提议：把第二条改成这样\n理由：……"
    );
    assert_eq!(found.user_id, user);
    client.resolve_comment(&c.id, true).await.unwrap();
    assert!(
        client
            .comments(&ws, doc_id)
            .await
            .unwrap()
            .iter()
            .any(|x| x.id == c.id && x.resolved)
    );
    let changes = client.comment_changes(&ws, doc_id, None).await.unwrap();
    assert!(
        changes.items.iter().any(|ch| ch.id == c.id),
        "{:?}",
        changes.items
    );
    client.delete_comment(&c.id).await.unwrap();
    assert!(
        !client
            .comments(&ws, doc_id)
            .await
            .unwrap()
            .iter()
            .any(|x| x.id == c.id)
    );

    // 登录不上（用一个不存在的账号，免得连着试错密码把测试账号锁住）
    let mut wrong = config;
    wrong.credentials.email = format!("nobody-{}@example.test", &ws[..8]);
    let err = Client::new(wrong).unwrap().sync().await.err().unwrap();
    assert!(matches!(err, Error::Auth(_)), "{err}");
}

/// 本地副本：一开始就断着网写本地，连上推上去；再断网写本地、同时别人在服务端改，重启以后连上，两边一样；之后收到的广播直接进库。
#[tokio::test]
async fn offline_replica_with_a_real_affine() {
    let Some(config) = config() else {
        eprintln!("没设 AFFINE_TEST_URL、AFFINE_TEST_EMAIL、AFFINE_TEST_PASSWORD，跳过");
        return;
    };
    let peer = config.url.clone();
    let client = Client::new(config).unwrap();
    let ws = client.create_workspace().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ws.db");
    let open = || Replica::new(affine_store::Store::open(&path).unwrap(), &ws, &peer).unwrap();
    let local_doc = |r: &mut Replica, id: &str| {
        let d = Doc::new();
        apply(&d, &r.read(id).unwrap().unwrap());
        d
    };

    // 还没连：根文档和一篇文档都只写在本地
    let doc_id = "tcmsOffline1";
    let mut r = open();
    let root = Doc::new();
    r.write(&ws, &add_page(&root, doc_id, "断网测试")).unwrap();
    let page = Doc::new();
    let md = "## 第一条\n\n开头\n\n## 第二条\n\n也是开头";
    affine_doc::create(
        &page,
        "断网测试",
        &affine_doc::to_blocks(md),
        &mut RandomIds,
    )
    .unwrap();
    r.write(
        doc_id,
        &page
            .transact()
            .encode_state_as_update_v1(&StateVector::default()),
    )
    .unwrap();
    assert_eq!(r.pending().unwrap().len(), 2);

    // 连上：两篇都推上去，另一条连接读得到
    let a = client.sync().await.unwrap();
    let report = r.sync_all(&a).await.unwrap();
    assert!(
        report.pushed.contains(&ws) && report.pushed.contains(&doc_id.to_owned()),
        "{report:?}"
    );
    assert!(r.pending().unwrap().is_empty());
    let b = client.sync().await.unwrap();
    let b_doc = Doc::new();
    apply(
        &b_doc,
        &b.load_doc(&ws, doc_id, None).await.unwrap().unwrap().update,
    );
    assert_eq!(markdown(&b_doc), md);
    let pages = {
        let d = Doc::new();
        apply(
            &d,
            &b.load_doc(&ws, &ws, None).await.unwrap().unwrap().update,
        );
        workspace::read_meta(&d).pages
    };
    assert!(pages.iter().any(|p| p.id == doc_id), "{pages:?}");

    // 断网（连接丢掉）：本地改第一条；同时别人在服务端改第二条
    drop(a);
    let edit = |doc: &Doc, n: usize, body: &str| {
        let it = affine_doc::split(&affine_doc::read(doc).unwrap().blocks)
            .into_iter()
            .filter_map(|s| match s {
                Section::Item(i) => Some(i),
                Section::Loose(_) => None,
            })
            .nth(n)
            .unwrap();
        let sv = doc.transact().state_vector();
        affine_doc::update_item(
            doc,
            it.heading_id.as_deref().unwrap(),
            &it.title,
            &affine_doc::to_blocks(body),
            &mut RandomIds,
        )
        .unwrap();
        doc.transact().encode_state_as_update_v1(&sv)
    };
    let mine = local_doc(&mut r, doc_id);
    r.write(doc_id, &edit(&mine, 0, "开头\n\n本地断网时写的"))
        .unwrap();
    let theirs = edit(&b_doc, 1, "也是开头\n\n别人同时在服务端写的");
    b.push_update(&ws, doc_id, &theirs, Intent::Update)
        .await
        .unwrap();
    assert_eq!(r.pending().unwrap(), [doc_id]);

    // 重启（库关掉再开），再连上：拉到别人的、推上自己的，两边一样
    drop(r);
    let mut r = open();
    assert_eq!(r.pending().unwrap(), [doc_id]);
    let mut a = client.sync().await.unwrap();
    let report = r.sync_all(&a).await.unwrap();
    assert!(
        report.pulled.contains(&doc_id.to_owned()) && report.pushed.contains(&doc_id.to_owned()),
        "{report:?}"
    );
    let merged = markdown(&local_doc(&mut r, doc_id));
    assert!(
        merged.contains("本地断网时写的") && merged.contains("别人同时在服务端写的"),
        "{merged}"
    );
    let sv = b_doc.transact().state_vector().encode_v1();
    apply(
        &b_doc,
        &b.load_doc(&ws, doc_id, Some(&sv))
            .await
            .unwrap()
            .unwrap()
            .update,
    );
    assert_eq!(markdown(&b_doc), merged);
    assert!(r.pending().unwrap().is_empty());

    // 连着的时候别人再改：广播直接进库，不用再拉
    let more = edit(
        &b_doc,
        1,
        "也是开头\n\n别人同时在服务端写的\n\n连着的时候又写的",
    );
    b.push_update(&ws, doc_id, &more, Intent::Update)
        .await
        .unwrap();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(20), a.next_event())
            .await
            .expect("等广播超时")
            .expect("连接没了");
        if matches!(&ev, SyncEvent::DocUpdates { doc_id: d, .. } if d == doc_id) {
            assert_eq!(r.apply_event(&ev).unwrap(), FollowUp::Nothing);
            break;
        }
    }
    assert_eq!(markdown(&local_doc(&mut r, doc_id)), markdown(&b_doc));
    let report = r.sync_all(&a).await.unwrap();
    assert!(
        report.pulled.is_empty() && report.pushed.is_empty(),
        "{report:?}"
    );
}
