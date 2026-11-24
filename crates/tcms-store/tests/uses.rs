//! 条目用了多少次（迁移 0006）：按天、按岗位加起来，只留 90 天，按最近几天合计。
//!
//! 需要环境变量 `TCMS_TEST_DATABASE_URL`：本地没设就跳过，CI 里没设算失败。

use tcms_store::{ItemUse, KEEP_DAYS, Store};

fn row(id: &str, role: &str, n: i32) -> (String, String, i32) {
    (id.to_owned(), role.to_owned(), n)
}

#[tokio::test]
async fn uses_add_up_by_day_and_role() {
    let Ok(url) = std::env::var("TCMS_TEST_DATABASE_URL") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI 里必须设 TCMS_TEST_DATABASE_URL"
        );
        eprintln!("没有设 TCMS_TEST_DATABASE_URL，跳过");
        return;
    };
    let schema = format!(
        "tcms_t_uses_{}",
        ulid::Ulid::generate().to_string().to_lowercase()
    );
    let store = Store::connect(&url, &schema).await.unwrap();

    // 同一次里同一条、同一个岗位出现两次，加在一起；再写一次接着加。0 次的不记。
    store
        .add_uses(
            "2026-11-20",
            &[
                row("a", "macro", 3),
                row("a", "macro", 2),
                row("a", "data", 1),
                row("b", "macro", 0),
            ],
        )
        .await
        .unwrap();
    store
        .add_uses("2026-11-20", &[row("a", "macro", 4)])
        .await
        .unwrap();
    store
        .add_uses("2026-11-22", &[row("a", "macro", 1), row("b", "data", 7)])
        .await
        .unwrap();
    let got = store.recent_uses("2026-11-22", 30).await.unwrap();
    assert_eq!(
        got,
        vec![
            ItemUse {
                item_id: "a".into(),
                uses: 11,
                days: 2,
                roles: vec!["data".into(), "macro".into()],
            },
            ItemUse {
                item_id: "b".into(),
                uses: 7,
                days: 1,
                roles: vec!["data".into()],
            },
        ]
    );
    // 只算最近几天（含今天）；今天以后的不算。
    let got = store.recent_uses("2026-11-22", 2).await.unwrap();
    assert_eq!(
        got.iter()
            .map(|u| (u.item_id.as_str(), u.uses))
            .collect::<Vec<_>>(),
        [("b", 7), ("a", 1)]
    );
    assert!(
        store
            .recent_uses("2026-11-19", 30)
            .await
            .unwrap()
            .is_empty()
    );

    // 写的时候删掉 90 天以前的。
    let later = "2027-02-19"; // 2026-11-20 往后 91 天
    store.add_uses(later, &[]).await.unwrap();
    let left = store.recent_uses(later, 400).await.unwrap();
    assert_eq!(
        left.iter()
            .map(|u| (u.item_id.as_str(), u.uses))
            .collect::<Vec<_>>(),
        [("b", 7), ("a", 1)]
    );
    assert_eq!(KEEP_DAYS, 90);

    store.drop_schema().await.unwrap();
}
