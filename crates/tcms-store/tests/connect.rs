//! 连真 PostgreSQL 的连接测试。
//!
//! 需要环境变量 `TCMS_TEST_DATABASE_URL`（能建角色和 schema 的账号）。没设时本地跳过；
//! 在 CI 里（设了 `CI`）没设就算失败，免得测试悄悄没跑。

use sqlx::postgres::PgPoolOptions;
use tcms_store::Store;

/// 和 I.P. 共用一个库时的账号：只拥有管理员给它建好的 schema，没有在库里建 schema 的权限
///（I.P. 仓库的 rust/deploy/postgres-init/01-roles.sh 就是这样建 tcms_app 的）。这样的账号要能连上、跑迁移。
#[tokio::test]
async fn connects_to_a_precreated_schema_without_database_create_privilege() {
    let Ok(url) = std::env::var("TCMS_TEST_DATABASE_URL") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI 里必须设 TCMS_TEST_DATABASE_URL"
        );
        eprintln!("没有设 TCMS_TEST_DATABASE_URL，跳过");
        return;
    };
    let suffix = ulid::Ulid::generate().to_string().to_lowercase();
    let role = format!("tcms_t_role_{suffix}");
    let schema = format!("tcms_t_owned_{suffix}");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("连接测试数据库");
    // 名字只含小写字母、数字、下划线；密码是测试里临时的，不是密钥。
    for sql in [
        format!("CREATE ROLE {role} LOGIN PASSWORD 'tcms_t_pw'"),
        format!("CREATE SCHEMA {schema} AUTHORIZATION {role}"),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&admin)
            .await
            .unwrap();
    }
    let db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await
        .unwrap();
    let can_create: bool = sqlx::query_scalar("SELECT has_database_privilege($1, $2, 'CREATE')")
        .bind(&role)
        .bind(&db)
        .fetch_one(&admin)
        .await
        .unwrap();

    let restricted = with_user(&url, &role, "tcms_t_pw");
    let result = Store::connect(&restricted, &schema).await;
    // 自己没有的 schema 还是建不了：报错，而不是悄悄用别的 schema。
    let other = Store::connect(&restricted, &format!("{schema}_other")).await;

    for sql in [
        format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
        format!("DROP ROLE IF EXISTS {role}"),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&admin)
            .await
            .unwrap();
    }
    assert!(
        !can_create,
        "新角色默认不该有建 schema 的权限，测试前提不成立"
    );
    let store = result.expect("schema 已建好时，没有建 schema 权限的账号也要能连上");
    assert_eq!(store.schema(), schema);
    assert!(other.is_err());
}

/// 把连接串里的用户名和密码换掉：postgres://旧用户:旧密码@主机/库 → postgres://新用户:新密码@主机/库。
fn with_user(url: &str, user: &str, password: &str) -> String {
    let (scheme, rest) = url.split_once("://").expect("连接串要有 ://");
    let host = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
    format!("{scheme}://{user}:{password}@{host}")
}
