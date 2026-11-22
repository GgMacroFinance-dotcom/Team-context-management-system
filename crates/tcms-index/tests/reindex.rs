//! 连真 PostgreSQL 的索引测试。
//!
//! 需要环境变量 `TCMS_TEST_DATABASE_URL`（能建 schema 的账号）。没设时本地跳过；
//! 在 CI 里（设了 `CI`）没设就算失败，免得测试悄悄没跑。每个测试用自己的 schema，跑完删掉。

use tcms_core::{ItemId, assign_ids};
use tcms_git::{Author, ContextRepo, FileEdit};
use tcms_index::reindex;
use tcms_store::Store;

async fn store() -> Option<Store> {
    let Ok(url) = std::env::var("TCMS_TEST_DATABASE_URL") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI 里必须设 TCMS_TEST_DATABASE_URL"
        );
        eprintln!("没有设 TCMS_TEST_DATABASE_URL，跳过");
        return None;
    };
    let schema = format!(
        "tcms_test_{}",
        ItemId::generate().as_str()[4..].to_lowercase()
    );
    Some(Store::connect(&url, &schema).await.expect("连接测试数据库"))
}

fn put(path: &str, text: &str) -> FileEdit {
    FileEdit {
        path: path.to_owned(),
        content: Some(text.as_bytes().to_vec()),
    }
}

const A: &str = "ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA";
const B: &str = "ctx_01JDX4M6Q8BBBBBBBBBBBBBBBB";
const C: &str = "ctx_01JDX4M6Q8CCCCCCCCCCCCCCCC";

#[tokio::test]
async fn full_then_incremental_then_idempotent() {
    let Some(store) = store().await else { return };
    let dir = tempfile::tempdir().unwrap();
    let repo = ContextRepo::init(dir.path()).unwrap();
    let author = Author::default();

    repo.commit(
        &[
            put("teams.yaml", "teams:\n  research:\n    roles: [macro]\n"),
            put(
                "org/规矩.md",
                &format!(
                    "---\nkind: rule\n---\n## 日期写全\n<!-- ctx id={A} -->\n写成 2026-11-22。\n\n## 还没 id\n正文\n"
                ),
            ),
            put(
                "team-research/决定.md",
                &format!("## 周报用表格\n<!-- ctx id={B} kind=decision review_by=2027-03-31 tags=周报 -->\n用表格。\n"),
            ),
            put("team-ops/x.md", &format!("## 没定义的团队\n<!-- ctx id={C} kind=fact -->\nx\n")),
        ],
        "第一版",
        &author,
    )
    .unwrap();

    let r = reindex(&repo, &store, false).await.unwrap();
    assert!(r.full);
    assert_eq!(r.files_parsed, 3);
    assert_eq!(r.counts.get("created"), Some(&3));
    let messages: Vec<&str> = r.warnings.iter().map(|w| w.message.as_str()).collect();
    assert!(
        messages.iter().any(|m| m.contains("还没有 id")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("team:ops")),
        "{messages:?}"
    );

    let b = store.get_item(B).await.unwrap().unwrap();
    assert_eq!(b.scope, "team:research");
    assert_eq!(b.kind, "decision");
    assert_eq!(b.authority, 2);
    assert_eq!(b.tags, vec!["周报"]);
    assert_eq!(b.review_by.as_deref(), Some("2027-03-31"));
    assert_eq!(b.revision, 1);
    assert_eq!(store.get_item(A).await.unwrap().unwrap().authority, 1);

    // 同一个提交再跑：什么都不做。全量再跑：没有变更。
    assert!(reindex(&repo, &store, false).await.unwrap().up_to_date);
    let again = reindex(&repo, &store, true).await.unwrap();
    assert_eq!(again.changed(), 0, "{:?}", again.counts);
    let before = store.all_items().await.unwrap();

    // 改一条、删一个文件、加一条取代 B 的新决定。
    let c2 = repo
        .commit(
            &[
                put(
                    "org/规矩.md",
                    &format!("---\nkind: rule\n---\n## 日期写全\n<!-- ctx id={A} -->\n写成 2026-11-22，不写“昨天”。\n"),
                ),
                FileEdit {
                    path: "team-ops/x.md".into(),
                    content: None,
                },
                put(
                    "team-research/新决定.md",
                    "## 周报改用图\n<!-- ctx id=ctx_01JDX4M6Q8DDDDDDDDDDDDDDDD kind=decision supersedes=ctx_01JDX4M6Q8BBBBBBBBBBBBBBBB -->\n用图。\n",
                ),
            ],
            "第二版",
            &author,
        )
        .unwrap();
    let r = reindex(&repo, &store, false).await.unwrap();
    assert!(!r.full);
    assert_eq!(r.files_parsed, 2);
    assert_eq!(r.commit.as_deref(), Some(c2.to_string().as_str()));
    assert_eq!(r.counts.get("updated"), Some(&1));
    assert_eq!(r.counts.get("archived"), Some(&1));
    assert_eq!(r.counts.get("superseded"), Some(&1));
    assert_eq!(r.counts.get("created"), Some(&1));

    let b = store.get_item(B).await.unwrap().unwrap();
    assert_eq!((b.status.as_str(), b.revision), ("superseded", 2));
    let c = store.get_item(C).await.unwrap().unwrap();
    assert_eq!((c.status.as_str(), c.revision), ("archived", 2));
    let a = store.get_item(A).await.unwrap().unwrap();
    assert_eq!(a.revision, 2);
    assert!(a.body.contains("不写"));

    // 旧版本的快照还在。
    let old = store.version_snapshot(A, 1).await.unwrap().unwrap();
    assert_eq!(old["body"], before.iter().find(|i| i.id == A).unwrap().body);

    // 变更序号按顺序递增，带上了范围。
    let changes = store.changes_since(0).await.unwrap();
    assert_eq!(changes.len(), 7);
    assert!(changes.windows(2).all(|w| w[0].seq < w[1].seq));
    assert_eq!(changes.last().unwrap().commit_id, c2.to_string());
    let since = changes[2].seq;
    assert_eq!(store.changes_since(since).await.unwrap().len(), 4);

    // 全量重建和增量结果一样。
    let after = store.all_items().await.unwrap();
    assert_eq!(reindex(&repo, &store, true).await.unwrap().changed(), 0);
    assert_eq!(store.all_items().await.unwrap(), after);

    store.drop_schema().await.unwrap();
}

#[tokio::test]
async fn assign_ids_then_index() {
    let Some(store) = store().await else { return };
    let dir = tempfile::tempdir().unwrap();
    let repo = ContextRepo::init(dir.path()).unwrap();
    let author = Author::default();
    let text =
        "---\nkind: term\n---\n## CPI\n居民消费价格指数。\n\n## PPI\n工业生产者出厂价格指数。\n";
    repo.commit(&[put("org/术语.md", text)], "术语", &author)
        .unwrap();
    let r = reindex(&repo, &store, false).await.unwrap();
    assert_eq!(r.changed(), 0);
    assert_eq!(r.warnings.len(), 2);

    let (new, ids) = assign_ids("org/术语.md", text).unwrap();
    assert_eq!(ids.len(), 2);
    repo.commit(&[put("org/术语.md", &new)], "补 id", &author)
        .unwrap();
    let r = reindex(&repo, &store, false).await.unwrap();
    assert_eq!(r.counts.get("created"), Some(&2));
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    let cpi = store.get_item(ids[0].0.as_str()).await.unwrap().unwrap();
    assert_eq!(cpi.title, "CPI");
    assert_eq!(cpi.kind, "term");

    store.drop_schema().await.unwrap();
}
