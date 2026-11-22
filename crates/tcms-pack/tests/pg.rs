//! 从 PostgreSQL 取条目、建检索索引、编上下文包的整条路。
//!
//! 需要环境变量 `TCMS_TEST_DATABASE_URL`，规则和 `tcms-index` 的集成测试一样：本地没设就跳过，
//! CI 里没设算失败。每个测试用自己的 schema，跑完删掉。

use tcms_core::ItemId;
use tcms_git::{Author, ContextRepo, FileEdit};
use tcms_pack::{Request, Section, compile};
use tcms_search::{Corpus, SearchIndex, SearchOptions, Visibility};
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
const D: &str = "ctx_01JDX4M6Q8DDDDDDDDDDDDDDDD";
const E: &str = "ctx_01JDX4M6Q8EEEEEEEEEEEEEEEE";

fn repo() -> (tempfile::TempDir, ContextRepo) {
    let dir = tempfile::tempdir().unwrap();
    let repo = ContextRepo::init(dir.path()).unwrap();
    repo.commit(
        &[
            put("teams.yaml", "teams:\n  research:\n    roles: [macro, data]\n"),
            put(
                "org/规矩.md",
                &format!("---\nkind: rule\n---\n## 数字要有来源\n<!-- ctx id={A} -->\n所有数字注明来源。\n"),
            ),
            put(
                "team-research/周报.md",
                &format!(
                    "## 周报用表格\n<!-- ctx id={B} kind=procedure review_by=2026-10-01 tags=\"周报, 格式\" -->\n\
                     周报用表格，日期写全。\n\n## 周报只写国内\n<!-- ctx id={C} kind=decision -->\n海外放月报。\n"
                ),
            ),
            put(
                "role-data/口径.md",
                &format!("## 数据岗位的周报口径\n<!-- ctx id={D} kind=fact -->\n别的岗位看不到。\n"),
            ),
            put(
                "org/简介.md",
                &format!("# 公司简介\n<!-- ctx id={E} kind=reference -->\n我们做宏观研究。\n"),
            ),
        ],
        "第一版",
        &Author::default(),
    )
    .unwrap();
    (dir, repo)
}

#[tokio::test]
async fn pack_from_postgres_is_reproducible() {
    let Some(first) = store().await else { return };
    let Some(second) = store().await else { return };
    let (_dir, repo) = repo();
    tcms_index::reindex(&repo, &first, false).await.unwrap();
    tcms_index::reindex(&repo, &second, true).await.unwrap();

    let vis = Visibility::agent("macro", Some("research"), None).unwrap();
    let req = Request {
        visibility: &vis,
        task: "写这周的周报",
        budget_tokens: 2000,
        today: "2026-11-23",
    };
    let corpus = Corpus::load(&first).await.unwrap();
    assert_eq!(corpus.len(), 5);
    let cursor = first.changes_since(0).await.unwrap().last().unwrap().seq;
    assert_eq!(corpus.cursor, cursor);
    assert_eq!(corpus.commit, repo.head().unwrap().map(|c| c.to_string()));

    let index = SearchIndex::build(corpus).unwrap();
    let a = compile(&index, &req).unwrap();
    assert_eq!(a.cursor, cursor);
    let sections: Vec<(&str, Section)> = a
        .included
        .iter()
        .map(|i| (i.id.as_str(), i.section))
        .collect();
    assert_eq!(
        sections,
        [(A, Section::Prefix), (B, Section::Task), (C, Section::Task)]
    );
    assert!(a.task.contains(&format!(
        "[{B} r1 · procedure · team:research · 待复核 2026-10-01]"
    )));
    assert_eq!(a.listed, [E]);
    assert!(!a.text().contains(D));

    // 另一个 schema 从同一个提交建的索引，编出来逐字节相同（变更序号各自的，不比）。
    let other = SearchIndex::build(Corpus::load(&second).await.unwrap()).unwrap();
    let b = compile(&other, &req).unwrap();
    assert_eq!(
        (&a.prefix, &a.task, &a.included),
        (&b.prefix, &b.task, &b.included)
    );

    // 检索：可见范围之外的不返回。
    let hits = index
        .search("周报口径", &vis, &SearchOptions::default())
        .unwrap();
    assert!(hits.iter().all(|h| h.id != D));
    assert_eq!(hits[0].id, B);
    let all = index
        .search("周报口径", &Visibility::All, &SearchOptions::default())
        .unwrap();
    assert!(all.iter().any(|h| h.id == D));

    first.drop_schema().await.unwrap();
    second.drop_schema().await.unwrap();
}
