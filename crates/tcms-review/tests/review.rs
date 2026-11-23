//! 审核页的接口：连真的 PostgreSQL，配一个临时的 git 仓库，直接把请求交给 axum 的路由（不开端口）。
//!
//! 需要环境变量 `TCMS_TEST_DATABASE_URL`，规则和别的集成测试一样：本地没设就跳过，CI 里没设算失败。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tcms_core::ItemId;
use tcms_git::{Author, ContextRepo, FileEdit};
use tcms_mcp::{HttpOptions, Live};
use tcms_review::{ReviewOptions, Writer, router};
use tcms_store::{Identity, NewToken, Store};
use tcms_write::{ProposeArgs, propose};
use tower::ServiceExt;

const RULE: &str = "ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA";
const MACRO: &str = "ctx_01JDX4M6Q8CCCCCCCCCCCCCCCC";
const OLD: &str = "ctx_01JDX4M6Q8DDDDDDDDDDDDDDDD";
const LATER: &str = "ctx_01JDX4M6Q8EEEEEEEEEEEEEEEE";

fn put(path: &str, text: &str) -> FileEdit {
    FileEdit {
        path: path.to_owned(),
        content: Some(text.as_bytes().to_vec()),
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    repo: ContextRepo,
    store: Store,
    live: Arc<Live>,
    app: Router,
    agent: Identity,
    agent_secret: String,
    reviewer: String,
}

async fn fixture() -> Option<Fixture> {
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
    let store = Store::connect(&url, &schema).await.expect("连接测试数据库");
    let dir = tempfile::tempdir().unwrap();
    let repo = ContextRepo::init(dir.path()).unwrap();
    repo.commit(
        &[
            put("teams.yaml", "teams:\n  research:\n    roles: [macro]\n"),
            put("org/规矩.md", &format!("## 数字要有来源\n<!-- ctx id={RULE} kind=rule -->\n所有数字注明来源。\n")),
            put(
                "role-macro/数据源.md",
                &format!("## 宏观数据从哪里取\n<!-- ctx id={MACRO} kind=fact tags=\"数据, 口径\" -->\nCPI 用国家统计局的数。\n"),
            ),
            put(
                "role-macro/日历.md",
                &format!(
                    "## 旧的发布日历\n<!-- ctx id={OLD} kind=reference review_by=2020-01-01 owner=alice -->\n2019 年的。\n\n\
                     ## 以后的发布日历\n<!-- ctx id={LATER} kind=reference review_by=2999-01-01 -->\n很久以后才要复核。\n"
                ),
            ),
        ],
        "第一版",
        &Author::default(),
    )
    .unwrap();
    tcms_index::reindex(&repo, &store, false).await.unwrap();
    let (_, agent_secret) = store
        .create_token(
            &NewToken {
                name: "I.P. 宏观".to_owned(),
                role: "macro".to_owned(),
                team: Some("research".to_owned()),
                projects: vec![],
                expires_in_days: None,
            },
            "test",
        )
        .await
        .unwrap();
    let agent = store.identify(&agent_secret).await.unwrap().unwrap();
    let (_, reviewer) = store
        .create_reviewer_token("王审核", None, "test")
        .await
        .unwrap();
    let live = Live::load(store.clone()).await.unwrap();
    let writer = Writer::spawn(&store, dir.path().to_owned()).unwrap();
    let app = router(
        live.clone(),
        writer,
        ReviewOptions {
            allowed_hosts: vec!["ctx.example.com".to_owned()],
            author_email: "review@example.com".to_owned(),
        },
    );
    Some(Fixture {
        path: dir.path().to_owned(),
        _dir: dir,
        repo,
        store,
        live,
        app,
        agent,
        agent_secret,
        reviewer,
    })
}

struct Reply {
    status: StatusCode,
    headers: http::HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|_| panic!("不是 JSON：{}", String::from_utf8_lossy(&self.body)))
    }
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Reply {
    send_host(app, method, path, token, body, "localhost:7341").await
}

async fn send_host(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
    host: &str,
) -> Reply {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, host);
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&b).unwrap())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let r = app.clone().oneshot(req).await.unwrap();
    let status = r.status();
    let headers = r.headers().clone();
    let body = r.into_body().collect().await.unwrap().to_bytes().to_vec();
    Reply {
        status,
        headers,
        body,
    }
}

async fn get(f: &Fixture, path: &str) -> Value {
    let r = send(&f.app, "GET", path, Some(&f.reviewer), None).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    r.json()
}

async fn post(f: &Fixture, path: &str, body: Value) -> Reply {
    send(&f.app, "POST", path, Some(&f.reviewer), Some(body)).await
}

async fn agent_proposes(f: &Fixture, v: Value) -> String {
    let index = f.live.current();
    let args: ProposeArgs = serde_json::from_value(v).unwrap();
    propose(&f.store, &index, &f.agent, args)
        .await
        .unwrap()
        .proposal_id
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("运行 git");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test]
async fn pages_login_and_hosts() {
    let Some(f) = fixture().await else { return };

    // 网页本身不用登录，带着安全相关的响应头。
    let r = send(&f.app, "GET", "/review", None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let csp = r.headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(csp.contains("script-src 'self'") && csp.contains("frame-ancestors 'none'"));
    assert_eq!(r.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert!(String::from_utf8_lossy(&r.body).contains("/review/app.js"));
    let r = send(&f.app, "GET", "/review/app.js", None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/javascript")
    );
    assert!(!String::from_utf8_lossy(&r.body).contains("innerHTML"));
    let r = send(&f.app, "GET", "/", None, None).await;
    assert!(r.status.is_redirection());

    // 接口要审核的人的令牌：没带、乱写、agent 的令牌都不行。
    for token in [None, Some("tcms_wrong"), Some(f.agent_secret.as_str())] {
        let r = send(&f.app, "GET", "/api/me", token, None).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{token:?}");
        assert_eq!(r.json()["code"], "login");
    }
    assert_eq!(get(&f, "/api/me").await["name"], "王审核");
    // agent 的令牌批准不了提议。
    let id = agent_proposes(&f, json!({"action": "create", "draft": {"title": "自己批自己", "body": "不行"}, "reason": "x"})).await;
    let r = send(
        &f.app,
        "POST",
        &format!("/api/proposals/{id}/accept"),
        Some(&f.agent_secret),
        Some(json!({})),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        f.store.get_proposal(&id).await.unwrap().unwrap().status,
        "open"
    );
    // 审核的人的令牌连不了 MCP。
    let mcp = tcms_mcp::router(f.live.clone(), HttpOptions::default());
    let r = send(
        &mcp,
        "POST",
        "/mcp",
        Some(&f.reviewer),
        Some(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    // Host 不在允许的列表里：拒绝（防 DNS 重绑定）。
    let r = send_host(
        &f.app,
        "GET",
        "/api/me",
        Some(&f.reviewer),
        None,
        "evil.example.net",
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = send_host(
        &f.app,
        "GET",
        "/api/me",
        Some(&f.reviewer),
        None,
        "ctx.example.com",
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);

    // 吊销以后马上不认。
    let (rid, secret) = f
        .store
        .create_reviewer_token("临时", Some(1), "test")
        .await
        .unwrap();
    assert_eq!(
        send(&f.app, "GET", "/api/me", Some(&secret), None)
            .await
            .status,
        StatusCode::OK
    );
    assert!(f.store.revoke_token(&rid, "test").await.unwrap());
    assert_eq!(
        send(&f.app, "GET", "/api/me", Some(&secret), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    // 令牌列表里分得清两种。
    let kinds: Vec<String> = f
        .store
        .list_tokens()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.kind)
        .collect();
    assert!(kinds.contains(&"agent".to_owned()) && kinds.contains(&"reviewer".to_owned()));

    f.store.drop_schema().await.unwrap();
}

#[tokio::test]
async fn proposals_accept_and_reject() {
    let Some(f) = fixture().await else { return };
    let create = agent_proposes(&f, json!({"action": "create", "draft": {"title": "PMI 用制造业的", "body": "PMI 默认指制造业 PMI。", "kind": "fact"}, "reason": "周报写错过"})).await;
    let update = agent_proposes(&f, json!({"action": "update", "item_id": MACRO, "draft": {"body": "CPI、PPI 都用国家统计局的数。"}, "reason": "PPI 也是"})).await;
    let near = agent_proposes(&f, json!({"action": "create", "draft": {"title": "宏观数据该从哪里取", "body": "换个说法。", "kind": "fact"}, "reason": "x"})).await;

    let list = get(&f, "/api/proposals").await;
    let ids: Vec<&str> = list["proposals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [create.as_str(), update.as_str(), near.as_str()]);
    assert_eq!(list["proposals"][2]["conflicts"], 1);
    assert_eq!(
        send(
            &f.app,
            "GET",
            "/api/proposals?status=nope",
            Some(&f.reviewer),
            None
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // 一条：差异、现在的样子、相近的条目带上现在的正文、默认写进哪个文件。
    let d = get(&f, &format!("/api/proposals/{update}")).await;
    assert_eq!(d["stale"], false);
    assert_eq!(d["current"]["revision"], 1);
    let ops: Vec<(&str, &str)> = d["diff"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| (l["op"].as_str().unwrap(), l["line"].as_str().unwrap()))
        .collect();
    assert!(
        ops.contains(&("removed", "CPI 用国家统计局的数。")),
        "{ops:?}"
    );
    assert!(
        ops.contains(&("added", "CPI、PPI 都用国家统计局的数。")),
        "{ops:?}"
    );
    let d = get(&f, &format!("/api/proposals/{near}")).await;
    assert_eq!(d["conflicts"][0]["id"], MACRO);
    assert_eq!(
        d["conflicts"][0]["current"]["body"],
        "CPI 用国家统计局的数。"
    );
    assert_eq!(d["default_file"], "role-macro/事实.md");
    assert_eq!(
        send(
            &f.app,
            "GET",
            "/api/proposals/prop_nope",
            Some(&f.reviewer),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    // 改了标题再接受：提交者是审核的人，提交信息里写着谁批的。
    let r = post(
        &f,
        &format!("/api/proposals/{create}/accept"),
        json!({"title": "PMI 指制造业 PMI", "note": "标题改清楚"}),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let a = r.json();
    assert_eq!(a["path"], "role-macro/事实.md");
    assert_eq!(a["proposal"]["status"], "accepted");
    let item = a["item"].as_str().unwrap().to_owned();
    let log = git(&f.path, &["log", "-1", "--format=%an <%ae>%n%B"]);
    assert!(
        log.starts_with("王审核 <review@example.com>\n新建：PMI 指制造业 PMI"),
        "{log}"
    );
    assert!(log.contains("Accepted-by: 王审核（tok_"), "{log}");
    assert!(f.live.refresh().await.unwrap().is_some());
    let found = get(&f, "/api/items?q=制造业").await;
    assert_eq!(found["items"][0]["id"], item.as_str());

    // 人在提议之后直接改了那一条：要写明看过第几版。
    f.repo
        .commit(
            &[put("role-macro/数据源.md", &format!("## 宏观数据从哪里取\n<!-- ctx id={MACRO} kind=fact tags=\"数据, 口径\" -->\nCPI 用国家统计局的数（月度）。\n"))],
            "人直接改",
            &Author::default(),
        )
        .unwrap();
    let r = post(&f, &format!("/api/proposals/{update}/accept"), json!({})).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(
        (r.json()["code"].clone(), r.json()["current"].clone()),
        (json!("stale"), json!(2))
    );
    let d = get(&f, &format!("/api/proposals/{update}")).await;
    assert_eq!(d["stale"], true);
    let r = post(
        &f,
        &format!("/api/proposals/{update}/accept"),
        json!({"over": 2, "body": "CPI、PPI 都用国家统计局的数（月度）。"}),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let row = f.store.get_item(MACRO).await.unwrap().unwrap();
    assert_eq!(
        (row.revision, row.body.as_str()),
        (3, "CPI、PPI 都用国家统计局的数（月度）。")
    );

    // 拒绝；处理过的不能再处理；参数不认识的挡下。
    let r = post(
        &f,
        &format!("/api/proposals/{near}/accept"),
        json!({"force": true}),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = post(
        &f,
        &format!("/api/proposals/{near}/reject"),
        json!({"note": "重复了"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["proposal"]["status"], "rejected");
    assert_eq!(r.json()["proposal"]["decision_note"], "重复了");
    let r = post(&f, &format!("/api/proposals/{near}/reject"), json!({})).await;
    assert_eq!(
        (r.status, r.json()["code"].clone()),
        (StatusCode::CONFLICT, json!("not_open"))
    );
    let r = post(&f, "/api/proposals/prop_nope/accept", json!({})).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let all = get(&f, "/api/proposals?status=all").await;
    assert_eq!(all["proposals"].as_array().unwrap().len(), 3);
    assert!(
        get(&f, "/api/proposals").await["proposals"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    f.store.drop_schema().await.unwrap();
}

#[tokio::test]
async fn due_browse_and_edit() {
    let Some(f) = fixture().await else { return };

    // 待复核：过了日期的才列，往后看几天的也能列。
    let due = get(&f, "/api/due").await;
    let ids: Vec<&str> = due["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [OLD]);
    assert_eq!(due["items"][0]["owner"], "alice");
    assert_eq!(
        get(&f, "/api/due?within=30").await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // 浏览：按范围、类型过滤，带每个范围、类型的条数。
    let all = get(&f, "/api/items").await;
    assert_eq!(all["total"], 4);
    assert!(
        all["scopes"]
            .as_array()
            .unwrap()
            .contains(&json!({"scope": "role:macro", "count": 3}))
    );
    let refs = get(&f, "/api/items?scope=role:macro&kind=reference").await;
    let ids: Vec<&str> = refs["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [LATER, OLD]);
    assert_eq!(
        send(
            &f.app,
            "GET",
            "/api/items?kind=gossip",
            Some(&f.reviewer),
            None
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    let one = get(&f, &format!("/api/items/{OLD}")).await;
    assert_eq!(one["item"]["review_by"], "2020-01-01");
    assert_eq!(
        send(
            &f.app,
            "GET",
            "/api/items/ctx_nope",
            Some(&f.reviewer),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    // 复核过了：改复核日期和正文，同文件里的另一条不动。
    let before = std::fs::read_to_string(f.path.join("role-macro/日历.md")).unwrap();
    let r = post(
        &f,
        &format!("/api/items/{OLD}/edit"),
        json!({"base_revision": 1, "review_by": "2999-06-01", "body": "2019 年的，看过了还有用。"}),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(r.json()["revision"], 2);
    let after = std::fs::read_to_string(f.path.join("role-macro/日历.md")).unwrap();
    let later_part = |s: &str| s[s.find("## 以后的发布日历").unwrap()..].to_owned();
    assert_eq!(later_part(&before), later_part(&after));
    assert!(after.contains("owner=alice"), "{after}");
    let row = f.store.get_item(OLD).await.unwrap().unwrap();
    assert_eq!(
        (row.review_by.as_deref(), row.body.as_str()),
        (Some("2999-06-01"), "2019 年的，看过了还有用。")
    );
    assert!(
        get(&f, "/api/due").await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let log = git(&f.path, &["log", "-1", "--format=%an%n%B"]);
    assert!(log.starts_with("王审核\n人改：旧的发布日历"), "{log}");
    assert!(
        log.contains(&format!("Item: {OLD}")) && log.contains("Edited-by: 王审核（tok_"),
        "{log}"
    );

    // 版本落后、没有改动、正文里藏二级标题：挡下，不提交。
    let commits = git(&f.path, &["rev-list", "--count", "HEAD"]);
    let r = post(
        &f,
        &format!("/api/items/{OLD}/edit"),
        json!({"base_revision": 1, "title": "别人刚改过"}),
    )
    .await;
    assert_eq!(
        (
            r.status,
            r.json()["code"].clone(),
            r.json()["current"].clone()
        ),
        (StatusCode::CONFLICT, json!("stale"), json!(2))
    );
    let r = post(
        &f,
        &format!("/api/items/{OLD}/edit"),
        json!({"base_revision": 2, "review_by": "2999-06-01"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.json()["error"].as_str().unwrap().contains("没有改动"));
    let r = post(
        &f,
        &format!("/api/items/{OLD}/edit"),
        json!({"base_revision": 2, "body": "正文\n\n## 偷偷多一条\n内容"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = post(
        &f,
        &format!("/api/items/{OLD}/edit"),
        json!({"base_revision": 2, "review_by": "明年"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(git(&f.path, &["rev-list", "--count", "HEAD"]), commits);

    // 人可以改规则（agent 不行）。
    let r = post(
        &f,
        &format!("/api/items/{RULE}/edit"),
        json!({"base_revision": 1, "body": "所有数字注明来源和日期。", "tags": ["来源"]}),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let rule = f.store.get_item(RULE).await.unwrap().unwrap();
    assert_eq!(
        (rule.kind.as_str(), rule.tags.clone()),
        ("rule", vec!["来源".to_owned()])
    );

    f.store.drop_schema().await.unwrap();
}
