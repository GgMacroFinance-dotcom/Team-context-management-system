//! 用 MCP 客户端（rmcp）连服务端测：stdio 那种字节流接法，和 Streamable HTTP。
//!
//! 需要环境变量 `TCMS_TEST_DATABASE_URL`，规则和别的集成测试一样：本地没设就跳过，CI 里没设算失败。

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ServiceExt, model::JsonObject};
use serde_json::{Value, json};
use tcms_core::ItemId;
use tcms_git::{Author, ContextRepo, FileEdit};
use tcms_mcp::{ContextServer, HttpOptions, Live, router};
use tcms_store::{NewToken, Store};

const RULE: &str = "ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA";
const WEEKLY: &str = "ctx_01JDX4M6Q8BBBBBBBBBBBBBBBB";
const MACRO: &str = "ctx_01JDX4M6Q8CCCCCCCCCCCCCCCC";
const DATA: &str = "ctx_01JDX4M6Q8DDDDDDDDDDDDDDDD";
const Q4: &str = "ctx_01JDX4M6Q8EEEEEEEEEEEEEEEE";
const Q1: &str = "ctx_01JDX4M6Q8FFFFFFFFFFFFFFFF";
const NEW: &str = "ctx_01JDX4M6Q8GGGGGGGGGGGGGGGG";
const NOWHERE: &str = "ctx_01JDX4M6Q8ZZZZZZZZZZZZZZZZ";

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

struct Fixture {
    _dir: tempfile::TempDir,
    repo: ContextRepo,
    store: Store,
    live: Arc<Live>,
    /// 宏观岗位、研究团队、能声明项目 q4。
    macro_token: String,
    /// 已经吊销的。
    revoked_token: String,
}

async fn fixture() -> Option<Fixture> {
    let store = store().await?;
    let dir = tempfile::tempdir().unwrap();
    let repo = ContextRepo::init(dir.path()).unwrap();
    repo.commit(
        &[
            put("teams.yaml", "teams:\n  research:\n    roles: [macro, data]\nprojects: [q4, q1]\n"),
            put("org/规矩.md", &format!("## 数字要有来源\n<!-- ctx id={RULE} kind=rule -->\n所有数字注明来源。\n")),
            put("team-research/周报.md", &format!("## 周报用表格\n<!-- ctx id={WEEKLY} kind=procedure -->\n周报用表格，日期写全。\n")),
            put("role-macro/数据源.md", &format!("## 宏观数据从哪里取\n<!-- ctx id={MACRO} kind=fact -->\nCPI 用国家统计局的数，周报里注明。\n")),
            put("role-data/口径.md", &format!("## 数据岗位的周报口径\n<!-- ctx id={DATA} kind=fact -->\n只有数据岗位看得到。\n")),
            put("project-q4/要求.md", &format!("## Q4 周报要求\n<!-- ctx id={Q4} kind=decision -->\nQ4 报告只用季度数据。\n")),
            put("project-q1/要求.md", &format!("## Q1 周报要求\n<!-- ctx id={Q1} kind=decision -->\nQ1 的。\n")),
        ],
        "第一版",
        &Author::default(),
    )
    .unwrap();
    tcms_index::reindex(&repo, &store, false).await.unwrap();
    let token = |role: &str, projects: &[&str]| NewToken {
        name: format!("测试-{role}"),
        role: role.to_owned(),
        team: Some("research".to_owned()),
        projects: projects.iter().map(|p| (*p).to_owned()).collect(),
        expires_in_days: None,
    };
    let (_, macro_token) = store
        .create_token(&token("macro", &["q4"]), "test")
        .await
        .unwrap();
    let (revoked_id, revoked_token) = store
        .create_token(&token("macro", &[]), "test")
        .await
        .unwrap();
    assert!(store.revoke_token(&revoked_id, "test").await.unwrap());
    assert!(!store.revoke_token(&revoked_id, "test").await.unwrap());
    let live = Live::load(store.clone()).await.unwrap();
    Some(Fixture {
        _dir: dir,
        repo,
        store,
        live,
        macro_token,
        revoked_token,
    })
}

/// 像 stdio 一样用一条字节流连上服务端。
async fn connect_stream(live: &Arc<Live>, secret: &str) -> RunningService<RoleClient, ()> {
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    let server = ContextServer::with_secret(live.clone(), secret.to_owned());
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    ().serve(client_io).await.expect("MCP 握手")
}

fn args(v: Value) -> JsonObject {
    match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

async fn call(client: &RunningService<RoleClient, ()>, name: &str, a: Value) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(args(a)))
        .await
        .expect("调用工具")
}

fn ok(r: &CallToolResult) -> &Value {
    assert_ne!(r.is_error, Some(true), "{r:?}");
    r.structured_content.as_ref().expect("有结构化结果")
}

fn err_text(r: &CallToolResult) -> String {
    assert_eq!(r.is_error, Some(true), "{r:?}");
    r.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect()
}

fn ids(v: &Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn tools_over_a_stream() {
    let Some(f) = fixture().await else { return };
    let client = connect_stream(&f.live, &f.macro_token).await;

    let info = client.peer_info().unwrap();
    assert!(
        info.instructions
            .as_deref()
            .unwrap()
            .contains("context_get")
    );
    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "context_changes",
            "context_get",
            "context_propose",
            "context_read",
            "context_search"
        ]
    );

    // 编包：令牌允许的项目 q4 能看到，数据岗位的看不到。
    let r = call(
        &client,
        "context_get",
        json!({"role": "macro", "project": "q4", "task": "写 Q4 的周报"}),
    )
    .await;
    let pack = ok(&r);
    let included = ids(pack, "included");
    assert_eq!(included[0], RULE);
    for id in [WEEKLY, MACRO, Q4] {
        assert!(
            included.iter().any(|x| x == id),
            "{id} 应该在包里：{included:?}"
        );
    }
    let text = format!(
        "{}{}",
        pack["prefix"].as_str().unwrap(),
        pack["task"].as_str().unwrap()
    );
    assert!(!text.contains(DATA) && !text.contains(Q1));
    assert_eq!(pack["cursor"], f.store.latest_cursor().await.unwrap());
    assert!(pack["warnings"].as_array().unwrap().is_empty());

    // 令牌不允许的项目和不存在的项目，回答一样。
    let denied = call(
        &client,
        "context_get",
        json!({"role": "macro", "project": "q1", "task": "周报"}),
    )
    .await;
    let absent = call(
        &client,
        "context_get",
        json!({"role": "macro", "project": "q9", "task": "周报"}),
    )
    .await;
    let (denied, absent) = (ok(&denied), ok(&absent));
    assert_eq!(denied["included"], absent["included"]);
    assert_eq!(denied["listed"], absent["listed"]);
    assert_eq!(denied["warnings"][0], "项目 q1 下没有你能看的条目");
    assert_eq!(absent["warnings"][0], "项目 q9 下没有你能看的条目");

    // 岗位、团队要和令牌一致。
    let r = call(
        &client,
        "context_get",
        json!({"role": "data", "task": "周报"}),
    )
    .await;
    assert!(err_text(&r).contains("这个令牌的岗位是 macro"));
    let r = call(
        &client,
        "context_get",
        json!({"role": "macro", "team": "ops", "task": "周报"}),
    )
    .await;
    assert!(err_text(&r).contains("这个令牌的团队是 research"));
    let r = call(
        &client,
        "context_get",
        json!({"role": "macro", "task": "周报", "budget_tokens": 5}),
    )
    .await;
    assert!(err_text(&r).contains("budget_tokens"));
    let r = call(&client, "context_get", json!({"task": "周报"})).await;
    assert!(err_text(&r).contains("参数不对"));

    // 检索：只在能看的范围里。
    let r = call(&client, "context_search", json!({"query": "周报"})).await;
    let hits = ids(ok(&r), "hits");
    assert!(hits.contains(&WEEKLY.to_owned()) && hits.contains(&Q4.to_owned()));
    assert!(!hits.contains(&DATA.to_owned()) && !hits.contains(&Q1.to_owned()));
    let r = call(
        &client,
        "context_search",
        json!({"query": "周报", "kind": "decision"}),
    )
    .await;
    assert_eq!(ids(ok(&r), "hits"), [Q4]);
    let r = call(
        &client,
        "context_search",
        json!({"query": "周报", "kind": "page"}),
    )
    .await;
    assert!(err_text(&r).contains("不认识的类型"));

    // 读单条：看不到的和不存在的回答一样。
    let r = call(&client, "context_read", json!({"id": WEEKLY})).await;
    let item = ok(&r);
    assert_eq!(item["title"], "周报用表格");
    assert_eq!(item["revision"], 1);
    assert_eq!(item["status"], "active");
    for id in [DATA, Q1, NOWHERE, "不是 id"] {
        let r = call(&client, "context_read", json!({"id": id})).await;
        assert_eq!(err_text(&r), format!("没有这一条：{id}"));
    }
    let r = call(
        &client,
        "context_read",
        json!({"id": WEEKLY, "revision": 9}),
    )
    .await;
    assert_eq!(err_text(&r), format!("没有这一条：{WEEKLY}"));

    // 变更：只有看得到的，cursor 跳过看不到的。
    let r = call(&client, "context_changes", json!({"since_cursor": 0})).await;
    let ch = ok(&r);
    let mut seen = ids(ch, "changes");
    seen.sort();
    assert_eq!(seen, [RULE, WEEKLY, MACRO, Q4]);
    assert_eq!(ch["cursor"], f.store.latest_cursor().await.unwrap());
    assert_eq!(ch["more"], false);
    let r = call(
        &client,
        "context_changes",
        json!({"since_cursor": 0, "limit": 1}),
    )
    .await;
    let first = ok(&r);
    assert_eq!(first["more"], true);
    assert_eq!(first["cursor"], first["changes"][0]["seq"]);

    // 不认识的工具是协议错误。
    assert!(
        client
            .call_tool(CallToolRequestParams::new("context_nope"))
            .await
            .is_err()
    );
    client.cancel().await.unwrap();

    // 吊销的令牌连工具列表都拿不到。
    let client = connect_stream(&f.live, &f.revoked_token).await;
    assert!(client.list_all_tools().await.is_err());
    client.cancel().await.unwrap();

    f.store.drop_schema().await.unwrap();
}

#[tokio::test]
async fn http_with_bearer_tokens() {
    let Some(f) = fixture().await else { return };
    let app = router(f.live.clone(), HttpOptions::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("http://{addr}/mcp");

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url.clone())
            .auth_header(f.macro_token.clone()),
    );
    let client = ().serve(transport).await.expect("HTTP 上握手");
    let r = call(
        &client,
        "context_get",
        json!({"role": "macro", "task": "周报"}),
    )
    .await;
    assert!(ids(ok(&r), "included").contains(&WEEKLY.to_owned()));
    client.cancel().await.unwrap();

    // 没带令牌、令牌不对、吊销了的：握手就失败（服务端回 401）。
    for auth in [
        None,
        Some("tcms_wrong".to_owned()),
        Some(f.revoked_token.clone()),
    ] {
        let mut config = StreamableHttpClientTransportConfig::with_uri(url.clone());
        if let Some(a) = auth {
            config = config.auth_header(a);
        }
        let transport = StreamableHttpClientTransport::from_config(config);
        assert!(().serve(transport).await.is_err());
    }

    f.store.drop_schema().await.unwrap();
}

#[tokio::test]
async fn follows_reindex_through_notify() {
    let Some(f) = fixture().await else { return };
    let follow = f.live.spawn_follow();
    let before = f.live.current().corpus().cursor;
    f.repo
        .commit(
            &[put(
                "role-macro/新的.md",
                &format!("## 新加的一条\n<!-- ctx id={NEW} kind=fact -->\n刚加的条目，讲 PMI。\n"),
            )],
            "加一条",
            &Author::default(),
        )
        .unwrap();
    tcms_index::reindex(&f.repo, &f.store, false).await.unwrap();
    // 收到 NOTIFY 以后换新索引；定时检查是 30 秒，这里等 10 秒以内就算是通知起的作用。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while f.live.current().corpus().cursor == before {
        assert!(
            tokio::time::Instant::now() < deadline,
            "10 秒内没换上新索引"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(f.live.current().corpus().get(NEW).is_some());
    let client = connect_stream(&f.live, &f.macro_token).await;
    let r = call(&client, "context_search", json!({"query": "PMI"})).await;
    assert_eq!(ids(ok(&r), "hits"), [NEW]);
    let r = call(&client, "context_changes", json!({"since_cursor": before})).await;
    assert_eq!(ids(ok(&r), "changes"), [NEW]);
    client.cancel().await.unwrap();
    // 没有新变更时不重建。
    assert!(f.live.refresh().await.unwrap().is_none());
    follow.abort();
    f.store.drop_schema().await.unwrap();
}

/// 条目换了范围以后读旧版本：现在这一版和要的那个版本都要看得到。
/// 测试的账号是超级用户时行级安全不起作用，这里看的是服务自己的那一道。
#[tokio::test]
async fn read_old_revision_after_scope_moves() {
    let Some(f) = fixture().await else { return };
    // RULE 从全公司挪到数据岗位，DATA 从数据岗位挪到全公司。
    f.repo
        .commit(
            &[
                put("org/规矩.md", &format!("## 数据岗位的周报口径\n<!-- ctx id={DATA} kind=fact -->\n现在全公司都看得到。\n")),
                put("role-data/口径.md", &format!("## 数字要有来源\n<!-- ctx id={RULE} kind=rule -->\n只给数据岗位了。\n")),
            ],
            "挪范围",
            &Author::default(),
        )
        .unwrap();
    tcms_index::reindex(&f.repo, &f.store, false).await.unwrap();
    let client = connect_stream(&f.live, &f.macro_token).await;
    // 现在看不到：第 1 版当时在全公司也不给。
    for revision in [None, Some(1), Some(2)] {
        let r = call(
            &client,
            "context_read",
            json!({"id": RULE, "revision": revision}),
        )
        .await;
        assert_eq!(err_text(&r), format!("没有这一条：{RULE}"));
    }
    // 现在看得到，第 1 版当时在数据岗位：只给现在的。
    let r = call(&client, "context_read", json!({"id": DATA})).await;
    assert_eq!(ok(&r)["scope"], "org");
    assert_eq!(ok(&r)["revision"], 2);
    let r = call(&client, "context_read", json!({"id": DATA, "revision": 1})).await;
    assert_eq!(err_text(&r), format!("没有这一条：{DATA}"));
    client.cancel().await.unwrap();
    f.store.drop_schema().await.unwrap();
}

/// agent 提议 → 批准以前不生效 → 人批准、写进 git → 服务换上新索引以后 agent 看得到。
#[tokio::test]
async fn propose_then_accept() {
    let Some(f) = fixture().await else { return };
    let client = connect_stream(&f.live, &f.macro_token).await;
    let propose = json!({
        "action": "create",
        "draft": {"title": "PMI 用制造业的", "body": "PMI 默认指制造业 PMI，非制造业的写明。", "kind": "fact", "tags": ["口径"]},
        "reason": "两次周报把非制造业 PMI 当成了 PMI",
        "evidence": [MACRO]
    });
    let r = call(&client, "context_propose", propose.clone()).await;
    let p = ok(&r).clone();
    assert_eq!(p["status"], "open");
    assert_eq!(p["duplicate"], false);
    assert_eq!(p["scope"], "role:macro");
    let id = p["proposal_id"].as_str().unwrap().to_owned();
    let item = p["result_item_id"].as_str().unwrap().to_owned();
    // 同样的再提一次：返回原来那条。
    let r = call(&client, "context_propose", propose).await;
    assert_eq!(ok(&r)["proposal_id"], id.as_str());
    assert_eq!(ok(&r)["duplicate"], true);

    // 挡下的：规则、看不到的条目、不认识的参数。
    let r = call(
        &client,
        "context_propose",
        json!({"action": "update", "item_id": RULE, "draft": {"body": "x"}, "reason": "x"}),
    )
    .await;
    assert!(err_text(&r).contains("规则只能人来改"));
    let r = call(
        &client,
        "context_propose",
        json!({"action": "archive", "item_id": DATA, "reason": "x"}),
    )
    .await;
    assert_eq!(err_text(&r), format!("没有这一条：{DATA}"));
    let r = call(
        &client,
        "context_propose",
        json!({"action": "create", "reason": "x", "force": true}),
    )
    .await;
    assert!(err_text(&r).starts_with("参数不对"));

    // 批准以前不在包里、搜不到。
    let r = call(&client, "context_search", json!({"query": "非制造业"})).await;
    assert!(ids(ok(&r), "hits").is_empty());

    // 人批准。
    let accepted = tcms_write::accept(
        &f.repo,
        &f.store,
        &id,
        &tcms_write::AcceptOptions {
            actor: "审核人".to_owned(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(accepted.item, item);
    assert!(f.live.refresh().await.unwrap().is_some());
    let r = call(&client, "context_search", json!({"query": "非制造业"})).await;
    assert_eq!(ids(ok(&r), "hits"), std::slice::from_ref(&item));
    let r = call(&client, "context_read", json!({"id": item})).await;
    assert_eq!(ok(&r)["commit"], accepted.commit.as_str());
    client.cancel().await.unwrap();
    f.store.drop_schema().await.unwrap();
}

#[tokio::test]
async fn token_lifecycle() {
    let Some(store) = store().await else { return };
    let new = NewToken {
        name: "I.P. 宏观".to_owned(),
        role: "macro".to_owned(),
        team: None,
        projects: vec![tcms_store::ALL_PROJECTS.to_owned()],
        expires_in_days: Some(30),
    };
    let (id, secret) = store.create_token(&new, "test").await.unwrap();
    assert!(id.starts_with("tok_") && secret.starts_with(tcms_store::TOKEN_PREFIX));
    let who = store
        .identify(&secret)
        .await
        .unwrap()
        .expect("新发的令牌认得");
    assert_eq!((who.role.as_str(), who.team.as_deref()), ("macro", None));
    assert!(who.all_projects() && who.may_use_project("任何"));
    // 换了大小写就不认。
    assert!(
        store
            .identify(&secret.to_uppercase())
            .await
            .unwrap()
            .is_none()
    );

    let rows = store.list_tokens().await.unwrap();
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r.id, id);
    assert!(r.expires_at.is_some() && r.revoked_at.is_none() && !r.expired);
    // 列表里没有明文和哈希。
    let json = serde_json::to_string(&rows).unwrap();
    assert!(!json.contains(&secret) && !json.contains(&tcms_store::hash_token(&secret)));

    assert!(store.revoke_token(&id, "test").await.unwrap());
    assert!(store.identify(&secret).await.unwrap().is_none());
    assert!(store.list_tokens().await.unwrap()[0].revoked_at.is_some());
    store.drop_schema().await.unwrap();
}
