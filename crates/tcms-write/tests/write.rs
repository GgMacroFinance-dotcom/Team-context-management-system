//! 提议、批准、拒绝：连真的 PostgreSQL，配一个临时的 git 仓库测。
//!
//! 需要环境变量 `TCMS_TEST_DATABASE_URL`，规则和别的集成测试一样：本地没设就跳过，CI 里没设算失败。
//! 查提交历史用 git 命令行（`git rev-list`、`git log`）。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use serde_json::{Value, json};
use tcms_core::ItemId;
use tcms_git::{Author, ContextRepo, FileEdit, Remote, RemoteError};
use tcms_search::{Corpus, SearchIndex};
use tcms_store::{Identity, NewToken, Store};
use tcms_write::{
    AcceptOptions, Committer, ItemEdit, MAX_OPEN_PER_AGENT, Proposal, ProposeArgs, ProposeError,
    WriteError, accept, edit_item, propose, reject, sync,
};

const RULE: &str = "ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA";
const WEEKLY: &str = "ctx_01JDX4M6Q8BBBBBBBBBBBBBBBB";
const MACRO: &str = "ctx_01JDX4M6Q8CCCCCCCCCCCCCCCC";
const DATA: &str = "ctx_01JDX4M6Q8DDDDDDDDDDDDDDDD";
const Q4: &str = "ctx_01JDX4M6Q8EEEEEEEEEEEEEEEE";
const Q1: &str = "ctx_01JDX4M6Q8FFFFFFFFFFFFFFFF";
const NOWHERE: &str = "ctx_01JDX4M6Q8ZZZZZZZZZZZZZZZZ";

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
    url: String,
    schema: String,
    /// 宏观岗位、研究团队、能声明项目 q4。
    macro_who: Identity,
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
            put("teams.yaml", "teams:\n  research:\n    roles: [macro, data]\nprojects: [q4, q1]\n"),
            put("org/规矩.md", &format!("## 数字要有来源\n<!-- ctx id={RULE} kind=rule -->\n所有数字注明来源。\n")),
            put("team-research/周报.md", &format!("## 周报用表格\n<!-- ctx id={WEEKLY} kind=procedure -->\n周报用表格，日期写全。\n")),
            put(
                "role-macro/数据源.md",
                &format!("# 数据源\n\n## 宏观数据从哪里取\n<!-- ctx id={MACRO} kind=fact tags=\"数据, 口径\" owner=alice -->\nCPI 用国家统计局的数，周报里注明。\n"),
            ),
            put("role-data/口径.md", &format!("## 数据岗位的周报口径\n<!-- ctx id={DATA} kind=fact -->\n只有数据岗位看得到。\n")),
            put("project-q4/要求.md", &format!("## Q4 周报要求\n<!-- ctx id={Q4} kind=decision -->\nQ4 报告只用季度数据。\n")),
            put("project-q1/要求.md", &format!("## Q1 周报要求\n<!-- ctx id={Q1} kind=decision -->\nQ1 的。\n")),
        ],
        "第一版",
        &Author::default(),
    )
    .unwrap();
    tcms_index::reindex(&repo, &store, false).await.unwrap();
    let macro_who = who(&store, "macro", &["q4"]).await;
    Some(Fixture {
        path: dir.path().to_owned(),
        _dir: dir,
        repo,
        store,
        url,
        schema,
        macro_who,
    })
}

/// 发一个令牌，换成它背后的身份。
async fn who(store: &Store, role: &str, projects: &[&str]) -> Identity {
    let (_, secret) = store
        .create_token(
            &NewToken {
                name: format!("测试-{role}"),
                role: role.to_owned(),
                team: Some("research".to_owned()),
                projects: projects.iter().map(|p| (*p).to_owned()).collect(),
                expires_in_days: None,
            },
            "test",
        )
        .await
        .unwrap();
    store.identify(&secret).await.unwrap().unwrap()
}

/// 服务手上那一份索引：照着库里现在的样子建。
async fn index(store: &Store) -> SearchIndex {
    SearchIndex::build(Corpus::load(store).await.unwrap()).unwrap()
}

fn args(v: Value) -> ProposeArgs {
    serde_json::from_value(v).unwrap()
}

async fn propose_ok(store: &Store, who: &Identity, v: Value) -> Proposal {
    let index = index(store).await;
    propose(store, &index, who, args(v)).await.expect("提议")
}

async fn propose_err(store: &Store, who: &Identity, v: Value) -> String {
    let index = index(store).await;
    match propose(store, &index, who, args(v)).await {
        Err(ProposeError::Invalid(msg)) => msg,
        other => panic!("应该被挡下：{other:?}"),
    }
}

fn opts() -> AcceptOptions {
    AcceptOptions {
        actor: "审核人".to_owned(),
        author: Author::default(),
        ..Default::default()
    }
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

fn commit_count(dir: &Path) -> usize {
    git(dir, &["rev-list", "--count", "HEAD"])
        .trim()
        .parse()
        .unwrap()
}

fn read(dir: &Path, path: &str) -> String {
    std::fs::read_to_string(dir.join(path)).unwrap()
}

#[tokio::test]
async fn propose_checks() {
    let Some(f) = fixture().await else { return };
    let (s, w) = (&f.store, &f.macro_who);
    let reason = "测试";

    // 规则只能人来改：新建、修改都不行。
    for v in [
        json!({"action": "create", "draft": {"title": "新规则", "body": "x", "kind": "rule"}, "reason": reason}),
        json!({"action": "update", "item_id": RULE, "draft": {"body": "改一下"}, "reason": reason}),
        json!({"action": "archive", "item_id": RULE, "reason": reason}),
    ] {
        assert!(propose_err(s, w, v).await.contains("规则只能人来改"));
    }
    // 只能往自己能写的范围提。
    for scope in ["role:data", "team:other", "project:q1"] {
        let msg = propose_err(s, w, json!({"action": "create", "draft": {"title": "t", "body": "b", "scope": scope}, "reason": reason})).await;
        assert!(msg.contains(&format!("不能往 {scope} 提")), "{msg}");
    }
    // 看不到的和不存在的回答一样。
    for id in [DATA, Q1, NOWHERE] {
        let msg = propose_err(
            s,
            w,
            json!({"action": "update", "item_id": id, "draft": {"body": "b"}, "reason": reason}),
        )
        .await;
        assert_eq!(msg, format!("没有这一条：{id}"));
    }
    // 参数本身不对的。
    for (v, want) in [
        (
            json!({"action": "create", "item_id": MACRO, "draft": {"title": "t", "body": "b"}, "reason": reason}),
            "不用写 item_id",
        ),
        (
            json!({"action": "update", "draft": {"body": "b"}, "reason": reason}),
            "要写 item_id",
        ),
        (
            json!({"action": "archive", "item_id": WEEKLY, "draft": {"body": "b"}, "reason": reason}),
            "不用写 draft",
        ),
        (
            json!({"action": "create", "draft": {"title": "t", "body": "b"}, "reason": "  "}),
            "reason 不能是空的",
        ),
        (
            json!({"action": "create", "draft": {"title": "", "body": "b"}, "reason": reason}),
            "title 不能是空的",
        ),
        (
            json!({"action": "create", "draft": {"title": "t"}, "reason": reason}),
            "body 不能是空的",
        ),
        (
            json!({"action": "supersede", "item_id": MACRO, "draft": {"title": "t"}, "reason": reason}),
            "要写新的正文",
        ),
        (
            json!({"action": "update", "item_id": MACRO, "draft": {"title": "宏观数据从哪里取"}, "reason": reason}),
            "没有要改的",
        ),
        (
            json!({"action": "update", "item_id": MACRO, "draft": {"scope": "org"}, "reason": reason}),
            "不能换范围",
        ),
        (
            json!({"action": "create", "draft": {"title": "t", "body": "b", "tags": ["a,b"]}, "reason": reason}),
            "标签最多",
        ),
        (
            json!({"action": "create", "draft": {"title": "t", "body": "b", "review_by": "明年"}, "reason": reason}),
            "review_by 不是日期",
        ),
        (
            json!({"action": "create", "draft": {"title": "t", "body": "b", "kind": "gossip"}, "reason": reason}),
            "gossip",
        ),
        (
            json!({"action": "rewrite", "draft": {"title": "t", "body": "b"}, "reason": reason}),
            "rewrite",
        ),
    ] {
        let msg = propose_err(s, w, v).await;
        assert!(msg.contains(want), "想要「{want}」，得到「{msg}」");
    }
    // 正文里藏一个二级标题，写进去以后会多出一条：挡下。
    let msg = propose_err(s, w, json!({"action": "create", "draft": {"title": "t", "body": "正文\n\n## 偷偷多一条\n内容"}, "reason": reason})).await;
    assert!(!msg.is_empty());
    // 不认识的参数直接报错（serde 那一层）。
    assert!(
        serde_json::from_value::<ProposeArgs>(
            json!({"action": "create", "reason": "x", "force": true})
        )
        .is_err()
    );

    // 默认：岗位范围、记忆。
    let p = propose_ok(s, w, json!({"action": "create", "draft": {"title": "CPI 发布日", "body": "每月中旬。"}, "reason": reason})).await;
    assert_eq!(
        (p.scope.as_str(), p.kind.as_str(), p.status.as_str()),
        ("role:macro", "memory", "open")
    );
    assert!(p.proposal_id.starts_with("prop_"));
    assert_eq!(p.result_item_id, format!("ctx_{}", &p.proposal_id[5..]));
    // 团队、允许的项目、org 都能提。
    for scope in ["team:research", "project:q4", "org"] {
        let p = propose_ok(s, w, json!({"action": "create", "draft": {"title": format!("{scope} 的一条"), "body": "b", "scope": scope}, "reason": reason})).await;
        assert_eq!(p.scope, scope);
    }
    // 修改时没写的字段从当时的条目补上。
    let p = propose_ok(s, w, json!({"action": "update", "item_id": MACRO, "draft": {"body": "CPI、PPI 都用国家统计局的数。"}, "reason": reason})).await;
    let row = s.get_proposal(&p.proposal_id).await.unwrap().unwrap();
    assert_eq!(row.title, "宏观数据从哪里取");
    assert_eq!(row.kind, "fact");
    assert_eq!(row.tags, ["数据", "口径"]);
    assert_eq!(row.base_revision, Some(1));
    assert_eq!(p.result_item_id, MACRO);

    f.store.drop_schema().await.unwrap();
}

#[tokio::test]
async fn duplicates_limits_conflicts_and_redaction() {
    let Some(f) = fixture().await else { return };
    let (s, w) = (&f.store, &f.macro_who);

    // 一样的内容换个理由再提：不新建，返回原来那条。
    let draft = json!({"title": "PMI 用制造业的", "body": "PMI 默认指制造业 PMI。"});
    let a = propose_ok(
        s,
        w,
        json!({"action": "create", "draft": draft, "reason": "第一次"}),
    )
    .await;
    let b = propose_ok(
        s,
        w,
        json!({"action": "create", "draft": draft, "reason": "换个说法"}),
    )
    .await;
    assert!(!a.duplicate && b.duplicate);
    assert_eq!(a.proposal_id, b.proposal_id);
    // 别的令牌提一样的内容也算重复。
    let other = who(s, "macro", &[]).await;
    let c = propose_ok(
        s,
        &other,
        json!({"action": "create", "draft": draft, "reason": "别人"}),
    )
    .await;
    assert!(c.duplicate);

    // 像密钥的东西抹掉，标题、正文、理由、证据都抹。
    let secret = "sk-proj-abcdefghijklmnop1234";
    let p = propose_ok(
        s,
        w,
        json!({"action": "create", "draft": {"title": "接口的 key", "body": format!("key 是 {secret}")}, "reason": format!("见 {secret}"), "evidence": [format!("日志里有 {secret}")]}),
    )
    .await;
    assert_eq!(p.redacted, 3);
    assert!(p.note.contains("3 处"));
    let row = s.get_proposal(&p.proposal_id).await.unwrap().unwrap();
    let stored = serde_json::to_string(&row).unwrap();
    assert!(!stored.contains(secret), "{stored}");
    assert!(row.body.contains(tcms_core::redact::MARK));

    // 相近的条目：同范围、同类型、标题相近。
    let p = propose_ok(s, w, json!({"action": "create", "draft": {"title": "宏观数据该从哪里取", "body": "换成新的口径。", "kind": "fact"}, "reason": "x"})).await;
    assert_eq!(p.conflicts.len(), 1, "{:?}", p.conflicts);
    assert_eq!(p.conflicts[0].id, MACRO);
    assert_eq!(p.conflicts[0].revision, 1);
    // 改那一条自己不算相近。
    let p = propose_ok(
        s,
        w,
        json!({"action": "update", "item_id": MACRO, "draft": {"body": "新的正文"}, "reason": "x"}),
    )
    .await;
    assert!(p.conflicts.is_empty());

    // 一个令牌最多攒 20 条没处理的。
    let busy = who(s, "macro", &[]).await;
    let mut first = None;
    for i in 0..MAX_OPEN_PER_AGENT {
        let p = propose_ok(s, &busy, json!({"action": "create", "draft": {"title": format!("第 {i} 条"), "body": "b"}, "reason": "x"})).await;
        first.get_or_insert(p.proposal_id);
    }
    let msg = propose_err(
        s,
        &busy,
        json!({"action": "create", "draft": {"title": "再来一条", "body": "b"}, "reason": "x"}),
    )
    .await;
    assert!(msg.contains("最多 20 条"), "{msg}");
    // 别的令牌不受影响；处理掉一条以后又能提。
    propose_ok(
        s,
        &other,
        json!({"action": "create", "draft": {"title": "别人的", "body": "b"}, "reason": "x"}),
    )
    .await;
    reject(s, first.as_deref().unwrap(), "审核人", None)
        .await
        .unwrap();
    propose_ok(
        s,
        &busy,
        json!({"action": "create", "draft": {"title": "再来一条", "body": "b"}, "reason": "x"}),
    )
    .await;

    f.store.drop_schema().await.unwrap();
}

#[tokio::test]
async fn accept_each_action() {
    let Some(f) = fixture().await else { return };
    let (s, w, dir) = (&f.store, &f.macro_who, f.path.as_path());

    // 新建：默认写进 role-macro/记忆.md。
    let p = propose_ok(s, w, json!({"action": "create", "draft": {"title": "CPI 发布日", "body": "每月中旬。", "tags": ["日历"]}, "reason": "大家老问"})).await;
    let a = accept(&f.repo, s, &p.proposal_id, &opts()).await.unwrap();
    assert!(!a.already_applied);
    assert_eq!(
        (a.item.as_str(), a.path.as_str()),
        (p.result_item_id.as_str(), "role-macro/记忆.md")
    );
    assert_eq!(a.proposal.status, "accepted");
    assert_eq!(a.proposal.decided_by.as_deref(), Some("审核人"));
    assert_eq!(a.proposal.result_commit.as_deref(), Some(a.commit.as_str()));
    let text = read(dir, "role-macro/记忆.md");
    assert_eq!(
        text,
        format!(
            "## CPI 发布日\n<!-- ctx id={} kind=memory tags=日历 -->\n\n每月中旬。\n",
            a.item
        )
    );
    let row = s.get_item(&a.item).await.unwrap().unwrap();
    assert_eq!(
        (row.status.as_str(), row.revision, row.commit_id.as_str()),
        ("active", 1, a.commit.as_str())
    );
    let msg = git(dir, &["log", "-1", "--format=%B"]);
    assert!(msg.starts_with("新建：CPI 发布日\n\n大家老问\n"), "{msg}");
    assert!(
        msg.contains(&format!("Proposal: {}", p.proposal_id)),
        "{msg}"
    );
    assert!(
        msg.contains(&format!("Proposed-by: 测试-macro ({})", w.token_id)),
        "{msg}"
    );
    assert!(msg.contains("Accepted-by: 审核人"), "{msg}");

    // 修改：标签、负责人照旧，版本号加一，同一个文件里别的东西不动。
    let p = propose_ok(s, w, json!({"action": "update", "item_id": MACRO, "draft": {"body": "CPI、PPI 都用国家统计局的数。"}, "reason": "PPI 也一样"})).await;
    let a = accept(&f.repo, s, &p.proposal_id, &opts()).await.unwrap();
    assert_eq!(a.item, MACRO);
    let row = s.get_item(MACRO).await.unwrap().unwrap();
    assert_eq!(
        (row.revision, row.body.as_str()),
        (2, "CPI、PPI 都用国家统计局的数。")
    );
    assert_eq!(
        (row.owner.as_deref(), row.tags.clone()),
        (Some("alice"), vec!["数据".to_owned(), "口径".to_owned()])
    );
    assert!(read(dir, "role-macro/数据源.md").starts_with("# 数据源\n\n## 宏观数据从哪里取\n"));

    // 取代：新的一条插在旧的后面，旧的变成被取代。
    let p = propose_ok(s, w, json!({"action": "supersede", "item_id": MACRO, "draft": {"title": "宏观数据的来源", "body": "统一走 Finance Macro Data API。"}, "reason": "换数据源了"})).await;
    let a = accept(&f.repo, s, &p.proposal_id, &opts()).await.unwrap();
    assert_eq!(a.path, "role-macro/数据源.md");
    let text = read(dir, "role-macro/数据源.md");
    let (old_at, new_at) = (text.find(MACRO).unwrap(), text.find(&a.item).unwrap());
    assert!(old_at < new_at, "{text}");
    assert!(text.contains(&format!("supersedes={MACRO}")), "{text}");
    assert_eq!(
        s.get_item(MACRO).await.unwrap().unwrap().status,
        "superseded"
    );
    let new = s.get_item(&a.item).await.unwrap().unwrap();
    assert_eq!(
        (new.kind.as_str(), new.supersedes.as_deref()),
        ("fact", Some(MACRO))
    );

    // 归档：文件里只剩它一条，整个文件删掉。
    let p = propose_ok(
        s,
        w,
        json!({"action": "archive", "item_id": WEEKLY, "reason": "不用表格了"}),
    )
    .await;
    let a = accept(&f.repo, s, &p.proposal_id, &opts()).await.unwrap();
    assert_eq!(
        (a.item.as_str(), a.path.as_str()),
        (WEEKLY, "team-research/周报.md")
    );
    assert!(!dir.join("team-research/周报.md").exists());
    assert_eq!(
        s.get_item(WEEKLY).await.unwrap().unwrap().status,
        "archived"
    );
    assert!(git(dir, &["log", "-1", "--format=%s"]).starts_with("归档：周报用表格"));

    // 人改了标题、换了文件再接受。
    let p = propose_ok(s, w, json!({"action": "create", "draft": {"title": "PMI", "body": "制造业的。", "scope": "project:q4"}, "reason": "x"})).await;
    let mut o = opts();
    o.title = Some("PMI 指制造业 PMI".to_owned());
    o.file = Some("project-q4/口径.md".to_owned());
    let a = accept(&f.repo, s, &p.proposal_id, &o).await.unwrap();
    assert_eq!(a.path, "project-q4/口径.md");
    assert_eq!(
        s.get_item(&a.item).await.unwrap().unwrap().title,
        "PMI 指制造业 PMI"
    );

    // 文件不在提议的范围里：不提交，提议还开着。
    let before = commit_count(dir);
    let p = propose_ok(
        s,
        w,
        json!({"action": "create", "draft": {"title": "放错地方", "body": "b"}, "reason": "x"}),
    )
    .await;
    let mut o = opts();
    o.file = Some("org/放错.md".to_owned());
    match accept(&f.repo, s, &p.proposal_id, &o).await {
        Err(WriteError::Invalid(msg)) => assert!(msg.contains("范围"), "{msg}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(commit_count(dir), before);
    assert_eq!(
        s.get_proposal(&p.proposal_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "open"
    );

    f.store.drop_schema().await.unwrap();
}

#[tokio::test]
async fn stale_target_gone_and_reject() {
    let Some(f) = fixture().await else { return };
    let (s, w, dir) = (&f.store, &f.macro_who, f.path.as_path());

    // 提议以后人直接改了仓库：停下来，写明看过第几版才接受。
    let p = propose_ok(s, w, json!({"action": "update", "item_id": MACRO, "draft": {"body": "提议的正文"}, "reason": "x"})).await;
    f.repo
        .commit(
            &[put(
                "role-macro/数据源.md",
                &format!("# 数据源\n\n## 宏观数据从哪里取\n<!-- ctx id={MACRO} kind=fact tags=\"数据, 口径\" owner=alice -->\n人改过的正文。\n"),
            )],
            "人直接改",
            &Author::default(),
        )
        .unwrap();
    match accept(&f.repo, s, &p.proposal_id, &opts()).await {
        Err(WriteError::Stale {
            item,
            base,
            current,
        }) => {
            assert_eq!((item.as_str(), base, current), (MACRO, 1, 2));
        }
        other => panic!("{other:?}"),
    }
    let mut o = opts();
    o.over_revision = Some(1);
    assert!(matches!(
        accept(&f.repo, s, &p.proposal_id, &o).await,
        Err(WriteError::Stale { .. })
    ));
    o.over_revision = Some(2);
    o.body = Some("合并以后的正文".to_owned());
    accept(&f.repo, s, &p.proposal_id, &o).await.unwrap();
    let row = s.get_item(MACRO).await.unwrap().unwrap();
    assert_eq!((row.revision, row.body.as_str()), (3, "合并以后的正文"));

    // 目标已经被人归档了：只能拒绝。
    let p = propose_ok(
        s,
        w,
        json!({"action": "update", "item_id": WEEKLY, "draft": {"body": "新"}, "reason": "x"}),
    )
    .await;
    f.repo
        .commit(
            &[FileEdit {
                path: "team-research/周报.md".to_owned(),
                content: None,
            }],
            "人删掉",
            &Author::default(),
        )
        .unwrap();
    assert!(matches!(
        accept(&f.repo, s, &p.proposal_id, &opts()).await,
        Err(WriteError::TargetGone(id)) if id == WEEKLY
    ));
    reject(s, &p.proposal_id, "审核人", Some("已经删了"))
        .await
        .unwrap();
    let row = s.get_proposal(&p.proposal_id).await.unwrap().unwrap();
    assert_eq!(
        (
            row.status.as_str(),
            row.decided_by.as_deref(),
            row.decision_note.as_deref()
        ),
        ("rejected", Some("审核人"), Some("已经删了"))
    );

    // 处理过的不能再处理，没有的说没有。
    assert!(matches!(
        accept(&f.repo, s, &p.proposal_id, &opts()).await,
        Err(WriteError::NotOpen { status, .. }) if status == "拒绝"
    ));
    assert!(matches!(
        reject(s, &p.proposal_id, "审核人", None).await,
        Err(WriteError::NotOpen { .. })
    ));
    assert!(matches!(
        accept(&f.repo, s, "prop_nope", &opts()).await,
        Err(WriteError::NotFound(_))
    ));
    // 拒绝不动 git。
    assert_eq!(commit_count(dir), 3 + 1);

    f.store.drop_schema().await.unwrap();
}

#[tokio::test]
async fn retry_after_half_finished_accept() {
    let Some(f) = fixture().await else { return };
    let (s, w, dir) = (&f.store, &f.macro_who, f.path.as_path());

    // 新建：提交了，但没来得及记下接受（这里手工做那个提交）。再批准一次：认得出来，不写第二遍。
    let p = propose_ok(s, w, json!({"action": "create", "draft": {"title": "CPI 发布日", "body": "每月中旬。"}, "reason": "x"})).await;
    f.repo
        .commit(
            &[put(
                "role-macro/记忆.md",
                &format!(
                    "## CPI 发布日\n<!-- ctx id={} kind=memory -->\n每月中旬。\n",
                    p.result_item_id
                ),
            )],
            "新建：CPI 发布日",
            &Author::default(),
        )
        .unwrap();
    let before = commit_count(dir);
    let a = accept(&f.repo, s, &p.proposal_id, &opts()).await.unwrap();
    assert!(a.already_applied);
    assert_eq!(a.item, p.result_item_id);
    assert_eq!(commit_count(dir), before);
    assert_eq!(a.proposal.status, "accepted");

    // 修改：仓库里已经是提议的样子了。
    let p = propose_ok(
        s,
        w,
        json!({"action": "update", "item_id": MACRO, "draft": {"body": "新正文。"}, "reason": "x"}),
    )
    .await;
    f.repo
        .commit(
            &[put(
                "role-macro/数据源.md",
                &format!("# 数据源\n\n## 宏观数据从哪里取\n<!-- ctx id={MACRO} kind=fact tags=\"数据, 口径\" owner=alice -->\n新正文。\n"),
            )],
            "修改：宏观数据从哪里取",
            &Author::default(),
        )
        .unwrap();
    let before = commit_count(dir);
    let a = accept(&f.repo, s, &p.proposal_id, &opts()).await.unwrap();
    assert!(a.already_applied);
    assert_eq!(commit_count(dir), before);

    // 归档：已经不在了。
    let p = propose_ok(
        s,
        w,
        json!({"action": "archive", "item_id": WEEKLY, "reason": "x"}),
    )
    .await;
    f.repo
        .commit(
            &[FileEdit {
                path: "team-research/周报.md".to_owned(),
                content: None,
            }],
            "归档：周报用表格",
            &Author::default(),
        )
        .unwrap();
    let a = accept(&f.repo, s, &p.proposal_id, &opts()).await.unwrap();
    assert!(a.already_applied);

    f.store.drop_schema().await.unwrap();
}

/// 好几个进程（各自的连接池、各自的仓库句柄）同时批准，每个进程里又同时批准好几条：
/// 一条一条排着提交，一个都不丢。
#[tokio::test]
async fn concurrent_accepts_keep_every_commit() {
    let Some(f) = fixture().await else { return };
    const PROCESSES: usize = 4;
    const EACH: usize = 3;
    let mut ids = Vec::new();
    for i in 0..PROCESSES * EACH {
        // 一半写进同一个文件，一半各写各的，同一个文件最容易丢改动。
        let p = propose_ok(&f.store, &f.macro_who, json!({"action": "create", "draft": {"title": format!("并发的第 {i} 条"), "body": format!("内容 {i}")}, "reason": "并发"})).await;
        ids.push((p.proposal_id, p.result_item_id, i % 2 == 1));
    }
    let before = commit_count(&f.path);

    let results: Vec<Vec<Result<String, String>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = ids
            .chunks(EACH)
            .map(|chunk| {
                let (url, schema, path) = (&f.url, &f.schema, &f.path);
                scope.spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async {
                        let store = Store::connect(url, schema).await.unwrap();
                        let repo = Rc::new(ContextRepo::open(path).unwrap());
                        let local = tokio::task::LocalSet::new();
                        let tasks: Vec<_> = chunk
                            .iter()
                            .map(|(id, item, own_file)| {
                                let (store, repo, id) = (store.clone(), repo.clone(), id.clone());
                                let mut o = opts();
                                if *own_file {
                                    o.file = Some(format!("role-macro/并发-{item}.md"));
                                }
                                local.spawn_local(async move {
                                    accept(&repo, &store, &id, &o)
                                        .await
                                        .map(|a| a.commit)
                                        .map_err(|e| e.to_string())
                                })
                            })
                            .collect();
                        local
                            .run_until(async {
                                let mut out = Vec::new();
                                for t in tasks {
                                    out.push(t.await.unwrap());
                                }
                                out
                            })
                            .await
                    })
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let commits: Vec<String> = results
        .into_iter()
        .flatten()
        .map(|r| r.expect("每一条都批准成功"))
        .collect();
    let distinct: std::collections::HashSet<&String> = commits.iter().collect();
    assert_eq!(distinct.len(), PROCESSES * EACH);
    assert_eq!(commit_count(&f.path), before + PROCESSES * EACH);
    // 每个提交都在 HEAD 的历史上，每一条都在文件里、在索引里。
    let history = git(&f.path, &["rev-list", "HEAD"]);
    for c in &commits {
        assert!(history.contains(c.as_str()), "{c} 不在历史上");
    }
    let shared = read(&f.path, "role-macro/记忆.md");
    for (id, item, own_file) in &ids {
        let text = if *own_file {
            read(&f.path, &format!("role-macro/并发-{item}.md"))
        } else {
            shared.clone()
        };
        assert!(text.contains(item.as_str()), "{item} 不在文件里");
        assert_eq!(
            f.store.get_item(item).await.unwrap().unwrap().status,
            "active"
        );
        assert_eq!(
            f.store.get_proposal(id).await.unwrap().unwrap().status,
            "accepted"
        );
    }
    assert!(
        git(&f.path, &["status", "--porcelain"]).is_empty(),
        "工作区要是干净的"
    );

    f.store.drop_schema().await.unwrap();
}

/// 人在自己的克隆里用 git：不签名、不跑钩子，提交者固定，和机器上的全局配置无关。
fn human_git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(["-c", "user.name=人", "-c", "user.email=human@example.com"])
        .args(["-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("运行 git");
    assert!(
        out.status.success(),
        "git {args:?}：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// 配了远端：批准前先拉到人推上去的改动，批准后推上去；合并不了时停下来、提议还是待审核；
/// 连不上远端时照样在本地接受；`sync` 把人推的改动拉进索引。远端是本机的裸仓库。
#[tokio::test]
async fn accept_and_edit_with_a_remote() {
    let Some(f) = fixture().await else { return };
    let (s, w, dir) = (&f.store, &f.macro_who, f.path.as_path());
    let remote_dir = tempfile::tempdir().unwrap();
    let base = remote_dir.path();
    human_git(base, &["init", "-q", "--bare", "remote.git"]);
    let bare = base.join("remote.git");
    let remote = Remote::new(bare.to_str().unwrap(), Some("main"), false).unwrap();
    let author = Author::default();

    // 第一次同步：远端是空的，推上去。
    let r = sync(&f.repo, s, &remote, &author).await.unwrap();
    assert!(r.errors.is_empty(), "{r:?}");
    assert!(r.pushed_new && !r.head_moved, "{r:?}");
    assert_eq!(
        human_git(&bare, &["rev-parse", "refs/heads/main"]),
        git(dir, &["rev-parse", "HEAD"]).trim()
    );

    // 人在自己的克隆里加了一条，推上去。
    human_git(base, &["clone", "-q", bare.to_str().unwrap(), "human"]);
    let human = base.join("human");
    const HUMAN: &str = "ctx_01JDX4M6Q8GGGGGGGGGGGGGGGG";
    std::fs::write(
        human.join("org/人写的.md"),
        format!("## 报告用中文\n<!-- ctx id={HUMAN} kind=rule -->\n报告一律用中文写。\n"),
    )
    .unwrap();
    human_git(&human, &["add", "."]);
    human_git(&human, &["commit", "-q", "-m", "人加了一条规矩"]);
    human_git(&human, &["push", "-q", "origin", "main"]);
    let human_head = human_git(&human, &["rev-parse", "HEAD"]);

    // 批准一条提议：先快进到人的提交（人的那条进了索引），提交在它后面，再推上去。
    let p = propose_ok(s, w, json!({"action": "update", "item_id": MACRO, "draft": {"body": "CPI、PPI 都用国家统计局的数。"}, "reason": "PPI 也一样"})).await;
    let mut o = opts();
    o.remote = Some(remote.clone());
    let a = accept(&f.repo, s, &p.proposal_id, &o).await.unwrap();
    let rep = a.remote.clone().expect("配了远端");
    assert!(rep.errors.is_empty(), "{rep:?}");
    assert!(
        rep.head_moved && rep.pushed_new && !rep.needs_merge,
        "{rep:?}"
    );
    assert!(
        rep.pulled.as_deref().unwrap().starts_with("快进"),
        "{rep:?}"
    );
    assert_eq!(
        git(dir, &["log", "-1", "--format=%P"]).trim(),
        human_head,
        "批准的提交接在人的提交后面"
    );
    assert_eq!(
        human_git(&bare, &["rev-parse", "refs/heads/main"]),
        a.commit
    );
    assert_eq!(s.get_item(HUMAN).await.unwrap().unwrap().status, "active");

    // 改单条也一样：先拉（快进到人刚推的改动）、后推。
    human_git(&human, &["pull", "-q", "--ff-only"]);
    std::fs::write(
        human.join("org/人写的.md"),
        format!(
            "## 报告用中文\n<!-- ctx id={HUMAN} kind=rule -->\n报告一律用中文写，术语可以附英文。\n"
        ),
    )
    .unwrap();
    human_git(&human, &["commit", "-q", "-am", "人改了规矩"]);
    human_git(&human, &["push", "-q", "origin", "main"]);
    let row = s.get_item(WEEKLY).await.unwrap().unwrap();
    let by = Committer {
        actor: "审核人".to_owned(),
        author: author.clone(),
        remote: Some(remote.clone()),
    };
    let change = ItemEdit {
        body: Some("周报用表格，日期写全，单位写明。".to_owned()),
        ..Default::default()
    };
    // 这里拿的是批准之前打开的仓库对象：拉的时候 git 命令行改了 HEAD，它照样读到新的。
    let e = edit_item(&f.repo, s, WEEKLY, row.revision, &change, &by)
        .await
        .unwrap();
    let rep = e.remote.expect("配了远端");
    assert!(rep.errors.is_empty(), "{rep:?}");
    assert!(
        rep.pulled.as_deref().unwrap().starts_with("快进"),
        "{rep:?}"
    );
    assert_eq!(
        human_git(&bare, &["rev-parse", "refs/heads/main"]),
        e.commit
    );
    assert_eq!(
        s.get_item(HUMAN).await.unwrap().unwrap().body,
        "报告一律用中文写，术语可以附英文。"
    );
    human_git(&human, &["pull", "-q", "--ff-only"]);
    assert_eq!(
        read(&human, "team-research/周报.md"),
        read(dir, "team-research/周报.md")
    );

    // 合并不了：服务这边离线时提交过一个改动（模拟），人在远端改了同一行。批准停下来，提议还是待审核。
    std::fs::write(
        human.join("org/人写的.md"),
        format!("## 报告用中文\n<!-- ctx id={HUMAN} kind=rule -->\n人：报告用中文。\n"),
    )
    .unwrap();
    human_git(&human, &["commit", "-q", "-am", "人又改了规矩"]);
    human_git(&human, &["push", "-q", "origin", "main"]);
    f.repo
        .commit(
            &[put(
                "org/人写的.md",
                &format!("## 报告用中文\n<!-- ctx id={HUMAN} kind=rule -->\n服务：报告用中文。\n"),
            )],
            "离线时的改动",
            &author,
        )
        .unwrap();
    let local_head = git(dir, &["rev-parse", "HEAD"]);
    let p = propose_ok(s, w, json!({"action": "create", "draft": {"title": "PMI 的口径", "body": "用制造业 PMI。"}, "reason": "常问"})).await;
    let err = accept(&f.repo, s, &p.proposal_id, &o).await.unwrap_err();
    match &err {
        WriteError::Remote(RemoteError::NeedsMerge { files, side, .. }) => {
            assert_eq!(files, &["org/人写的.md"]);
            assert_eq!(side.as_deref(), Some("tcms/main-unmerged"));
        }
        other => panic!("应该合并不了：{other}"),
    }
    assert_eq!(git(dir, &["rev-parse", "HEAD"]), local_head);
    assert_eq!(git(dir, &["status", "--porcelain"]), "");
    let still = s.get_proposal(&p.proposal_id).await.unwrap().unwrap();
    assert_eq!(still.status, "open");
    let r = sync(&f.repo, s, &remote, &author).await.unwrap();
    assert!(r.needs_merge && r.pushed.is_none(), "{r:?}");

    // 人在服务器的仓库里合并好以后，再批准就过了，一起推上去。
    human_git(dir, &["fetch", "-q", bare.to_str().unwrap(), "main"]);
    let merged = Command::new("git")
        .args([
            "-c",
            "commit.gpgSign=false",
            "-c",
            "user.name=人",
            "-c",
            "user.email=human@example.com",
        ])
        .args(["merge", "-q", "--no-edit", "FETCH_HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(!merged.status.success(), "这里应该有冲突");
    std::fs::write(
        dir.join("org/人写的.md"),
        format!("## 报告用中文\n<!-- ctx id={HUMAN} kind=rule -->\n合并：报告用中文。\n"),
    )
    .unwrap();
    human_git(dir, &["add", "org/人写的.md"]);
    human_git(dir, &["commit", "-q", "--no-edit"]);
    let a = accept(&f.repo, s, &p.proposal_id, &o).await.unwrap();
    let rep = a.remote.expect("配了远端");
    assert!(rep.errors.is_empty() && rep.pushed_new, "{rep:?}");
    assert_eq!(
        human_git(&bare, &["rev-parse", "refs/heads/main"]),
        a.commit
    );
    assert_eq!(
        s.get_item(HUMAN).await.unwrap().unwrap().body,
        "合并：报告用中文。"
    );

    // 连不上远端：照样在本地接受，错误记在结果里。
    let gone = Remote::new(
        base.join("没有这个仓库.git").to_str().unwrap(),
        Some("main"),
        false,
    )
    .unwrap();
    let p = propose_ok(s, w, json!({"action": "create", "draft": {"title": "M2 的口径", "body": "用月末值。"}, "reason": "常问"})).await;
    let mut o = opts();
    o.remote = Some(gone);
    let a = accept(&f.repo, s, &p.proposal_id, &o).await.unwrap();
    assert_eq!(a.proposal.status, "accepted");
    let rep = a.remote.expect("配了远端");
    assert_eq!(rep.errors.len(), 2, "{rep:?}");
    assert!(
        rep.errors[0].starts_with("拉取：") && rep.errors[1].starts_with("推送："),
        "{rep:?}"
    );
    assert!(rep.pulled.is_none() && rep.pushed.is_none());
    // 恢复以后同步一次就补推上去了。
    let r = sync(&f.repo, s, &remote, &author).await.unwrap();
    assert!(r.errors.is_empty() && r.pushed_new, "{r:?}");
    assert_eq!(
        human_git(&bare, &["rev-parse", "refs/heads/main"]),
        a.commit
    );

    f.store.drop_schema().await.unwrap();
}
