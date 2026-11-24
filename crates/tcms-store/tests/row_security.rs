//! 行级安全（迁移 0005，`docs/architecture.md` 第 5 节）：agent 的读在数据库这一层也只看得到令牌的范围。
//!
//! 超级用户不受行级安全管，所以这里像部署时一样，用一个普通账号（只拥有自己的 schema）连。
//! 需要环境变量 `TCMS_TEST_DATABASE_URL`（能建角色和 schema 的账号）。没设时本地跳过；在 CI 里没设就算失败。

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use tcms_store::{ChangeKind, Identity, ItemRow, ItemWrite, Store};

const PASSWORD: &str = "tcms_t_pw";

#[tokio::test]
async fn agent_reads_are_limited_by_row_security() {
    let Ok(url) = std::env::var("TCMS_TEST_DATABASE_URL") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI 里必须设 TCMS_TEST_DATABASE_URL"
        );
        eprintln!("没有设 TCMS_TEST_DATABASE_URL，跳过");
        return;
    };
    let suffix = ulid::Ulid::generate().to_string().to_lowercase();
    let role = format!("tcms_t_rls_{suffix}");
    let schema = format!("tcms_t_rls_{suffix}");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("连接测试数据库");
    // 名字只含小写字母、数字、下划线；密码是测试里临时的，不是密钥。
    for sql in [
        format!("CREATE ROLE {role} LOGIN PASSWORD '{PASSWORD}'"),
        format!("CREATE SCHEMA {schema} AUTHORIZATION {role}"),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&admin)
            .await
            .unwrap();
    }
    // 出错也要收拾干净：检查放在另一个任务里，panic 了在这里接住。
    let checks = tokio::spawn(checks(url.clone(), role.clone(), schema.clone())).await;
    for sql in [
        format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
        format!("DROP ROLE IF EXISTS {role}"),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&admin)
            .await
            .unwrap();
    }
    if let Err(e) = checks {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn checks(url: String, role: String, schema: String) {
    let app_url = with_user(&url, &role, PASSWORD);
    let store = Store::connect(&app_url, &schema)
        .await
        .expect("普通账号连上自己的 schema");
    assert!(!store.bypasses_row_security().await.unwrap());

    // 第一个提交：各个范围一条；F 先在全公司，G 先在 legal 岗位。
    let mut tx = store.begin_index(None).await.unwrap();
    tx.write(&[
        write("A", "org", 1),
        write("B", "role:macro", 1),
        write("C", "role:legal", 1),
        write("D", "team:research", 1),
        write("E", "project:q4", 1),
        write("F", "org", 1),
        write("G", "role:legal", 1),
    ])
    .await
    .unwrap();
    tx.finish("c1").await.unwrap();
    // 第二个提交：F 挪到 legal 岗位，G 挪到全公司。
    let mut tx = store.begin_index(Some("c1")).await.unwrap();
    tx.write(&[write("F", "role:legal", 2), write("G", "org", 2)])
        .await
        .unwrap();
    tx.finish("c2").await.unwrap();

    // 服务自己读（建索引、审核页、命令行）不拦。
    assert_eq!(store.all_items().await.unwrap().len(), 7);
    assert_eq!(store.snapshot().await.unwrap().items.len(), 7);

    let macro_ = who("macro", Some("research"), &[]);
    let legal = who("legal", None, &[]);
    let any_project = who("macro", Some("research"), &["*"]);

    // context_read：store 这里不按范围写条件，看不到的是数据库拦下的。
    for id in ["A", "B", "D", "G"] {
        assert!(read(&store, &macro_, id, None).await.is_some(), "{id}");
    }
    for id in ["C", "E", "F", "nope"] {
        assert!(read(&store, &macro_, id, None).await.is_none(), "{id}");
    }
    assert!(read(&store, &any_project, "E", None).await.is_some());
    // 现在的这一版看不到：旧版本当时在全公司也不给。
    assert!(read(&store, &macro_, "F", Some(1)).await.is_none());
    let f = read(&store, &legal, "F", Some(1)).await.unwrap();
    assert_eq!(f.current.scope, "role:legal");
    assert_eq!(f.version.unwrap().scope, "org");
    // 现在的这一版看得到，旧版本当时在 legal 岗位：旧版本看不到。
    let g = read(&store, &macro_, "G", Some(1)).await.unwrap();
    assert_eq!(g.current.scope, "org");
    assert!(g.version.is_none());
    assert!(
        read(&store, &legal, "G", Some(1))
            .await
            .unwrap()
            .version
            .is_some()
    );

    // context_changes：只有看得到的范围；cursor 还是全部变更里最大的序号。
    let ch = store.visible_changes(&macro_, 0, 100).await.unwrap();
    let mut seen: Vec<(String, String)> = ch
        .changes
        .iter()
        .map(|c| (c.id.clone(), c.scope.clone()))
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        [
            ("A", "org"),
            ("B", "role:macro"),
            ("D", "team:research"),
            ("F", "org"),
            ("G", "org")
        ]
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
    );
    assert_eq!(ch.cursor, store.latest_cursor().await.unwrap());
    // F 那次变更在全公司，但 F 现在在 legal 岗位：不带标题。
    let f_change = ch.changes.iter().find(|c| c.id == "F").unwrap();
    assert_eq!(f_change.title, None);
    assert_eq!(
        ch.changes
            .iter()
            .find(|c| c.id == "A")
            .unwrap()
            .title
            .as_deref(),
        Some("标题 A")
    );

    // 直接写 SQL、不带任何条件：写上范围以后，三张表都只出来这些范围的行。
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            app_url
                .parse::<sqlx::postgres::PgConnectOptions>()
                .unwrap()
                .options([("search_path", schema.as_str())]),
        )
        .await
        .unwrap();
    assert_eq!(counts(&pool, None).await, (7, 9, 9));
    assert_eq!(counts(&pool, Some(("{org}", "off"))).await, (2, 3, 3));
    assert_eq!(
        counts(&pool, Some(("{org,role:macro}", "off"))).await,
        (3, 4, 4)
    );
    assert_eq!(counts(&pool, Some(("{org}", "on"))).await, (3, 4, 4));
    assert_eq!(counts(&pool, Some(("{}", "off"))).await, (0, 0, 0));
    // 范围只在那个事务里有效，同一个连接的下一个事务不受影响。
    assert_eq!(counts(&pool, None).await, (7, 9, 9));
    // 写上范围以后，看不到的行也改不了。
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('tcms.scopes', '{org}', true)")
        .execute(&mut *tx)
        .await
        .unwrap();
    let changed = sqlx::query("UPDATE items SET title = 'x' WHERE id = 'C'")
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(changed, 0);
    tx.rollback().await.unwrap();
    pool.close().await;

    // 超级用户不受行级安全管：数据库不拦，只剩调用的地方自己查（MCP 的 context_read 就是这样）。
    let admin = Store::connect(&url, &schema).await.unwrap();
    if admin.bypasses_row_security().await.unwrap() {
        assert!(read(&admin, &macro_, "C", None).await.is_some());
    }
}

/// 写上范围（`None` 是不写）以后，`items`、`item_versions`、`changes` 各数出几行。
async fn counts(pool: &PgPool, reach: Option<(&str, &str)>) -> (i64, i64, i64) {
    let mut tx = pool.begin().await.unwrap();
    if let Some((scopes, all_projects)) = reach {
        sqlx::query(
            "SELECT set_config('tcms.scopes', $1, true), set_config('tcms.all_projects', $2, true)",
        )
        .bind(scopes)
        .bind(all_projects)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    let row = sqlx::query(
        "SELECT (SELECT count(*) FROM items), (SELECT count(*) FROM item_versions), \
                (SELECT count(*) FROM changes)",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    (row.get(0), row.get(1), row.get(2))
}

async fn read(
    store: &Store,
    who: &Identity,
    id: &str,
    revision: Option<i32>,
) -> Option<tcms_store::ItemRead> {
    store.read_as(who, id, revision).await.unwrap()
}

fn who(role: &str, team: Option<&str>, projects: &[&str]) -> Identity {
    Identity {
        token_id: "tok_test".to_owned(),
        name: "测试".to_owned(),
        role: role.to_owned(),
        team: team.map(str::to_owned),
        projects: projects.iter().map(|p| (*p).to_owned()).collect(),
    }
}

fn write(id: &str, scope: &str, revision: i32) -> ItemWrite {
    ItemWrite {
        row: ItemRow {
            id: id.to_owned(),
            scope: scope.to_owned(),
            kind: "fact".to_owned(),
            authority: 1,
            title: format!("标题 {id}"),
            body: format!("正文 {id} {revision}"),
            tags: Vec::new(),
            owner: None,
            review_by: None,
            supersedes: None,
            status: "active".to_owned(),
            revision,
            content_hash: format!("h{id}{revision}"),
            path: "x.md".to_owned(),
            commit_id: format!("c{revision}"),
        },
        change: if revision == 1 {
            ChangeKind::Created
        } else {
            ChangeKind::Updated
        },
    }
}

/// 把连接串里的用户名和密码换掉：postgres://旧用户:旧密码@主机/库 → postgres://新用户:新密码@主机/库。
fn with_user(url: &str, user: &str, password: &str) -> String {
    let (scheme, rest) = url.split_once("://").expect("连接串要有 ://");
    let host = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
    format!("{scheme}://{user}:{password}@{host}")
}
