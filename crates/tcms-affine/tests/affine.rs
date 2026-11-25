//! 对着真的 AFFiNE（自托管，0.27.5 起）和 PostgreSQL 测同步一次、一直跟着同步。要设：
//!
//! - `AFFINE_TEST_URL`、`AFFINE_TEST_EMAIL`、`AFFINE_TEST_PASSWORD`（和 `affine-sync-client` 的测试一样）；
//! - `TCMS_TEST_DATABASE_URL`。
//!
//! 没设就跳过（CI 里没有 AFFiNE）。每跑一次新建一个工作区，不删。
//! “人在 AFFiNE 里改”用另一条同步连接推一段修改来模拟（和网页编辑器推的一样是 Yjs 更新）。

use std::cell::RefCell;
use std::time::Duration;

use affine_sync_client::{Client, Config, Credentials, Intent, SyncConnection};
use tcms_affine::workspace::{self, DocMeta, NodeKind};
use tcms_affine::{
    Context, FollowEnv, FollowEvent, FollowOptions, Job, RandomIds, Section, Server, follow, link,
    split, sync, to_blocks,
};
use tcms_core::{ItemId, Scope};
use tcms_git::{Author, ContextRepo, FileEdit};
use tcms_store::Store;
use yrs::updates::decoder::Decode;
use yrs::{Doc, ReadTxn, StateVector, Transact, Update};

const A: &str = "ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA";
const B: &str = "ctx_01JDX4M6Q8BBBBBBBBBBBBBBBB";
const PATH: &str = "role-macro/数据源.md";

fn config() -> Option<(Config, String)> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let config = Config {
        url: var("AFFINE_TEST_URL")?,
        credentials: Credentials {
            email: var("AFFINE_TEST_EMAIL")?,
            password: var("AFFINE_TEST_PASSWORD")?,
        },
        client_version: None,
        extra_ca: None,
    };
    Some((config, var("TCMS_TEST_DATABASE_URL")?))
}

async fn load(conn: &SyncConnection, ws: &str, id: &str) -> Doc {
    let doc = Doc::new();
    let loaded = conn.load_doc(ws, id, None).await.unwrap().unwrap();
    doc.transact_mut()
        .apply_update(Update::decode_v1(&loaded.update).unwrap())
        .unwrap();
    doc
}

/// 把 `doc` 从 `sv` 以后的改动推上去（模拟人在网页里改）。
async fn push_since(conn: &SyncConnection, ws: &str, id: &str, doc: &Doc, sv: &StateVector) {
    let delta = doc.transact().encode_state_as_update_v1(sv);
    conn.push_update(ws, id, &delta, Intent::Update)
        .await
        .unwrap();
}

/// 等到 `ok()`，最多 30 秒。
async fn wait_for<F: std::future::Future<Output = bool>>(what: &str, ok: impl Fn() -> F) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !ok().await {
        assert!(tokio::time::Instant::now() < deadline, "等不到：{what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn items(doc: &Doc) -> Vec<(String, String, String)> {
    split(&tcms_affine::read(doc).unwrap().blocks)
        .into_iter()
        .filter_map(|s| match s {
            Section::Item(i) => Some((i.heading_id.clone()?, i.title.clone(), i.body_markdown())),
            Section::Loose(_) => None,
        })
        .collect()
}

#[tokio::test]
async fn sync_with_a_real_affine() {
    let Some((config, url)) = config() else {
        eprintln!(
            "没设 AFFINE_TEST_URL、AFFINE_TEST_EMAIL、AFFINE_TEST_PASSWORD、TCMS_TEST_DATABASE_URL，跳过"
        );
        return;
    };
    let schema = format!(
        "tcms_test_{}",
        ItemId::generate().as_str()[4..].to_lowercase()
    );
    let store = Store::connect(&url, &schema).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let repo = ContextRepo::init(dir.path()).unwrap();
    let text = format!(
        "# 数据源\n\n宏观岗位用的数据。\n\n## CPI 从哪里取\n<!-- ctx id={A} kind=fact -->\n\nCPI 用国家统计局的数。\n\n- 月度\n- 同比\n\n## PMI\n<!-- ctx id={B} kind=fact -->\n\nPMI 用统计局和财新两个。\n"
    );
    repo.commit(
        &[FileEdit {
            path: PATH.to_owned(),
            content: Some(text.into_bytes()),
        }],
        "第一版",
        &Author::default(),
    )
    .unwrap();
    tcms_index::reindex(&repo, &store, false).await.unwrap();

    let mut server = Server::connect(config.clone()).await.unwrap();
    let ws = server.create_workspace("TCMS 同步测试").await.unwrap();
    let author = Author::default();
    let cx = Context {
        repo: &repo,
        store: &store,
        workspace: &ws,
        actor: "测试",
        remote: None,
        author: &author,
    };
    let scope: Scope = "role:macro".parse().unwrap();
    let r = link(&cx, &mut server, &scope, None).await.unwrap();
    assert_eq!(r.docs.len(), 1);
    assert_eq!(r.docs[0].state, "linked");
    let doc_id = r.docs[0].doc_id.clone().unwrap();

    // 另一条连接（“人”）看到的：文档列表里有它，role-macro 文件夹里链着它，两条都在。
    let human = Client::new(config).unwrap().sync().await.unwrap();
    let root = load(&human, &ws, &ws).await;
    let meta = workspace::read_meta(&root);
    assert_eq!(meta.name.as_deref(), Some("TCMS 同步测试"));
    assert!(
        meta.pages
            .iter()
            .any(|p| p.id == doc_id && p.title == "数据源"),
        "{meta:?}"
    );
    let folders = workspace::folders(&load(&human, &ws, &tcms_affine::folders_doc_id(&ws)).await);
    let folder = folders
        .iter()
        .find(|n| n.kind == NodeKind::Folder && n.data == "role-macro")
        .unwrap();
    assert!(
        folders
            .iter()
            .any(|n| n.parent_id.as_deref() == Some(folder.id.as_str()) && n.data == doc_id)
    );
    let page = load(&human, &ws, &doc_id).await;
    let got = items(&page);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].2, "CPI 用国家统计局的数。\n\n- 月度\n- 同比");

    // 人在 AFFiNE 里改第一条：同步以后进 git，作者是 AFFiNE 里的名字。
    let sv = page.transact().state_vector();
    tcms_affine::update_item(
        &page,
        &got[0].0,
        "CPI 从哪里取",
        &to_blocks("CPI 用统计局的数，**季调后**。"),
        &mut RandomIds,
    )
    .unwrap();
    let delta = page.transact().encode_state_as_update_v1(&sv);
    human
        .push_update(&ws, &doc_id, &delta, Intent::Update)
        .await
        .unwrap();
    let r = sync(&cx, &mut server, &[]).await.unwrap();
    assert_eq!(r.docs[0].to_git.updated, 1, "{r:?}");
    assert_eq!(r.commits.len(), 1);
    let file = std::fs::read_to_string(dir.path().join(PATH)).unwrap();
    assert!(file.contains("CPI 用统计局的数，**季调后**。"), "{file}");
    assert!(file.contains("PMI 用统计局和财新两个。"), "{file}");
    let log = std::process::Command::new("git")
        .args(["log", "-1", "--format=%an <%ae>"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let who = String::from_utf8(log.stdout).unwrap();
    assert!(
        who.trim().ends_with("<affine@tcms.invalid>") && !who.starts_with("AFFiNE"),
        "{who}"
    );

    // git 里改第二条、删第一条以外的都不动：同步以后人那边读到。
    let file = file.replace("PMI 用统计局和财新两个。", "PMI 只用统计局的。");
    repo.commit(
        &[FileEdit {
            path: PATH.to_owned(),
            content: Some(file.into_bytes()),
        }],
        "人改的",
        &Author::default(),
    )
    .unwrap();
    let r = sync(&cx, &mut server, &[]).await.unwrap();
    assert_eq!(r.docs[0].to_affine.updated, 1, "{r:?}");
    let page = load(&human, &ws, &doc_id).await;
    let got = items(&page);
    assert_eq!(got[1].2, "PMI 只用统计局的。");
    assert_eq!(got[0].2, "CPI 用统计局的数，**季调后**。");

    // 再同步一次：两边都没变。
    let r = sync(&cx, &mut server, &[]).await.unwrap();
    assert_eq!(r.docs[0].state, "unchanged", "{r:?}");
    assert!(r.commits.is_empty());
    // 服务端的这篇文档和本系统记下的一样（没有漏推的）。
    let docs = store.affine_docs().await.unwrap();
    let ours = Doc::new();
    ours.transact_mut()
        .apply_update(Update::decode_v1(&docs[0].state).unwrap())
        .unwrap();
    let missing = page
        .transact()
        .encode_state_as_update_v1(&ours.transact().state_vector());
    let check = Doc::new();
    check
        .transact_mut()
        .apply_update(Update::decode_v1(&missing).unwrap())
        .unwrap();
    assert_eq!(check.transact().state_vector(), StateVector::default());
    store.drop_schema().await.unwrap();
}

/// 一直跟着同步：网页那边（另一条连接）改了，靠广播进 git；那边新建一篇文档以后（之前的加入作废了）再改，照样收得到；
/// git 里新加的文件接上以后，网页那边改它也收得到；git 里改了，不用等，进 AFFiNE。
#[tokio::test]
async fn follow_a_real_affine() {
    let Some((config, url)) = config() else {
        eprintln!(
            "没设 AFFINE_TEST_URL、AFFINE_TEST_EMAIL、AFFINE_TEST_PASSWORD、TCMS_TEST_DATABASE_URL，跳过"
        );
        return;
    };
    let schema = format!(
        "tcms_test_{}",
        ItemId::generate().as_str()[4..].to_lowercase()
    );
    let store = Store::connect(&url, &schema).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let repo = ContextRepo::init(dir.path()).unwrap();
    let text = format!(
        "# 数据源\n\n## CPI 从哪里取\n<!-- ctx id={A} kind=fact -->\n\nCPI 用国家统计局的数。\n\n## PMI\n<!-- ctx id={B} kind=fact -->\n\nPMI 用统计局和财新两个。\n"
    );
    repo.commit(
        &[FileEdit {
            path: PATH.to_owned(),
            content: Some(text.into_bytes()),
        }],
        "第一版",
        &Author::default(),
    )
    .unwrap();
    tcms_index::reindex(&repo, &store, false).await.unwrap();

    let mut server = Server::connect(config.clone()).await.unwrap();
    let ws = server.create_workspace("TCMS 跟着同步测试").await.unwrap();
    let author = Author::default();
    let cx = Context {
        repo: &repo,
        store: &store,
        workspace: &ws,
        actor: "测试",
        remote: None,
        author: &author,
    };
    let scope: Scope = "role:macro".parse().unwrap();
    let r = link(&cx, &mut server, &scope, None).await.unwrap();
    let doc_id = r.docs[0].doc_id.clone().unwrap();
    let human = Client::new(config).unwrap().sync().await.unwrap();

    let env = FollowEnv {
        repo: dir.path().to_path_buf(),
        store: store.clone(),
        workspace: ws.clone(),
        actor: "测试".to_owned(),
        remote: None,
        author: Author::default(),
    };
    let ms = Duration::from_millis;
    let opts = FollowOptions {
        quiet: ms(500),
        max_wait: ms(5000),
        git_poll: ms(300),
        // 测试里不靠定时全部看一遍：改动要靠广播收到。
        full_every: Duration::from_secs(3600),
        retry: ms(1000),
    };
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let log: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let file = || std::fs::read_to_string(dir.path().join(PATH)).unwrap();

    let script = async {
        wait_for("第一次全部看一遍", || async {
            !log.borrow().is_empty()
        })
        .await;

        // 网页那边改第一条：收到广播，停一下以后进 git。
        let page = load(&human, &ws, &doc_id).await;
        let sv = page.transact().state_vector();
        let h = items(&page)[0].0.clone();
        tcms_affine::update_item(
            &page,
            &h,
            "CPI 从哪里取",
            &to_blocks("CPI 用统计局的数，**季调后**。"),
            &mut RandomIds,
        )
        .unwrap();
        push_since(&human, &ws, &doc_id, &page, &sv).await;
        wait_for("网页改的进 git", || async {
            file().contains("**季调后**")
        })
        .await;

        // 网页那边在 role-macro 文件夹里新建一篇：进 git 成一个新文件。
        let new_id = "follow-test-new-doc";
        let new_doc = Doc::new();
        tcms_affine::create(
            &new_doc,
            "新文档",
            &to_blocks("## 新条目\n\n网页里新建的。"),
            &mut RandomIds,
        )
        .unwrap();
        let full = new_doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        human
            .push_update(&ws, new_id, &full, Intent::Create)
            .await
            .unwrap();
        let root = load(&human, &ws, &ws).await;
        let sv = root.transact().state_vector();
        workspace::upsert_doc_meta(&root, &DocMeta::new(new_id, "新文档", 0.0));
        push_since(&human, &ws, &ws, &root, &sv).await;
        let folders_id = tcms_affine::folders_doc_id(&ws);
        let folders = load(&human, &ws, &folders_id).await;
        let sv = folders.transact().state_vector();
        let folder = workspace::folders(&folders)
            .into_iter()
            .find(|n| n.kind == NodeKind::Folder && n.data == "role-macro")
            .unwrap();
        workspace::create_link(&folders, &folder.id, NodeKind::Doc, new_id, &mut RandomIds)
            .unwrap();
        push_since(&human, &ws, &folders_id, &folders, &sv).await;
        let new_file = dir.path().join("role-macro/新文档.md");
        wait_for("新建的文档进 git", || async { new_file.exists() }).await;

        // 新建过文档（之前的加入作废了）以后，网页那边再改第一篇：照样收到广播。
        let page = load(&human, &ws, &doc_id).await;
        let sv = page.transact().state_vector();
        let h = items(&page)[1].0.clone();
        tcms_affine::update_item(
            &page,
            &h,
            "PMI",
            &to_blocks("PMI 用统计局、财新，**两个都看**。"),
            &mut RandomIds,
        )
        .unwrap();
        push_since(&human, &ws, &doc_id, &page, &sv).await;
        wait_for("新建文档以后网页改的也进 git", || async {
            file().contains("**两个都看**")
        })
        .await;

        // git 里新加一个文件：接上，建成文档；网页那边改它，照样收到广播（新建的文档也加入了）。
        const C: &str = "ctx_01JDX4M6Q8CCCCCCCCCCCCCCCC";
        let other = "role-macro/口径.md";
        repo.commit(
            &[FileEdit {
                path: other.to_owned(),
                content: Some(
                    format!("## 口径\n<!-- ctx id={C} kind=fact -->\n\n同比。\n").into_bytes(),
                ),
            }],
            "人加的",
            &Author::default(),
        )
        .unwrap();
        tcms_index::reindex(&repo, &store, false).await.unwrap();
        let linked_id = || async {
            store
                .affine_docs()
                .await
                .unwrap()
                .into_iter()
                .find(|d| d.path == other)
                .map(|d| d.doc_id)
        };
        wait_for("git 里新加的文件接上", || async {
            linked_id().await.is_some()
        })
        .await;
        let other_id = linked_id().await.unwrap();
        let page = load(&human, &ws, &other_id).await;
        let sv = page.transact().state_vector();
        let h = items(&page)[0].0.clone();
        tcms_affine::update_item(
            &page,
            &h,
            "口径",
            &to_blocks("同比、环比。"),
            &mut RandomIds,
        )
        .unwrap();
        push_since(&human, &ws, &other_id, &page, &sv).await;
        wait_for("网页改新接上的文档进 git", || async {
            std::fs::read_to_string(dir.path().join(other))
                .unwrap()
                .contains("同比、环比。")
        })
        .await;

        // git 里改第一条：不用等，网页那边读到。
        let text = file().replace("**季调后**", "**季调以后**");
        repo.commit(
            &[FileEdit {
                path: PATH.to_owned(),
                content: Some(text.into_bytes()),
            }],
            "人改的",
            &Author::default(),
        )
        .unwrap();
        tcms_index::reindex(&repo, &store, false).await.unwrap();
        wait_for("git 里改的进 AFFiNE", || async {
            let page = load(&human, &ws, &doc_id).await;
            items(&page)[0].2 == "CPI 用统计局的数，**季调以后**。"
        })
        .await;
        stop.send(true).unwrap();
    };
    let follower = follow(&env, &mut server, opts, stopped, |ev| {
        let line = match &ev {
            FollowEvent::Synced(job, r) => format!(
                "{}:{}",
                if matches!(job, Job::Full) {
                    "full"
                } else {
                    "docs"
                },
                r.docs
                    .iter()
                    .filter(|d| d.state != "unchanged")
                    .map(|d| format!("{} {}", d.path, d.state))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            FollowEvent::Failed(e) => format!("failed:{e}"),
            FollowEvent::Disconnected(r) => format!("disconnected:{r}"),
            FollowEvent::Reconnected => "reconnected".to_owned(),
        };
        eprintln!("跟着同步：{line}");
        log.borrow_mut().push(line);
    });
    tokio::time::timeout(Duration::from_secs(180), async {
        tokio::join!(follower, script);
    })
    .await
    .expect("跟着同步没停下来");
    let log = log.into_inner();
    assert!(!log.iter().any(|l| l.starts_with("failed")), "{log:?}");
    assert_eq!(
        log.iter().filter(|l| l.starts_with("full")).count(),
        1,
        "{log:?}"
    );
    store.drop_schema().await.unwrap();
}
