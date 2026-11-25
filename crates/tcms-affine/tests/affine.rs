//! 对着真的 AFFiNE（自托管，0.27.5 起）和 PostgreSQL 测同步一次。要设：
//!
//! - `AFFINE_TEST_URL`、`AFFINE_TEST_EMAIL`、`AFFINE_TEST_PASSWORD`（和 `affine-sync-client` 的测试一样）；
//! - `TCMS_TEST_DATABASE_URL`。
//!
//! 没设就跳过（CI 里没有 AFFiNE）。每跑一次新建一个工作区，不删。
//! “人在 AFFiNE 里改”用另一条同步连接推一段修改来模拟（和网页编辑器推的一样是 Yjs 更新）。

use affine_sync_client::{Client, Config, Credentials, Intent, SyncConnection};
use tcms_affine::workspace::{self, NodeKind};
use tcms_affine::{Context, RandomIds, Section, Server, link, split, sync, to_blocks};
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

async fn load(conn: &mut SyncConnection, ws: &str, id: &str) -> Doc {
    let doc = Doc::new();
    let loaded = conn.load_doc(ws, id, None).await.unwrap().unwrap();
    doc.transact_mut()
        .apply_update(Update::decode_v1(&loaded.update).unwrap())
        .unwrap();
    doc
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
    let mut human = Client::new(config).unwrap().sync().await.unwrap();
    let root = load(&mut human, &ws, &ws).await;
    let meta = workspace::read_meta(&root);
    assert_eq!(meta.name.as_deref(), Some("TCMS 同步测试"));
    assert!(
        meta.pages
            .iter()
            .any(|p| p.id == doc_id && p.title == "数据源"),
        "{meta:?}"
    );
    let folders =
        workspace::folders(&load(&mut human, &ws, &tcms_affine::folders_doc_id(&ws)).await);
    let folder = folders
        .iter()
        .find(|n| n.kind == NodeKind::Folder && n.data == "role-macro")
        .unwrap();
    assert!(
        folders
            .iter()
            .any(|n| n.parent_id.as_deref() == Some(folder.id.as_str()) && n.data == doc_id)
    );
    let page = load(&mut human, &ws, &doc_id).await;
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
    let page = load(&mut human, &ws, &doc_id).await;
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
