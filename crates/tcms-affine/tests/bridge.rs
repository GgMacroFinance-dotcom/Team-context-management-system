//! 同步一次、一直跟着同步：连真的 PostgreSQL、一个临时的 git 仓库，AFFiNE 换成内存里的假 AFFiNE（只存 Yjs 文档，收发更新）。
//! 人在 AFFiNE 里的修改用 `affine-doc` 直接改假 AFFiNE 里的文档来模拟，跟着同步时再发一个广播。
//!
//! 需要环境变量 `TCMS_TEST_DATABASE_URL`，规则和别的集成测试一样：本地没设就跳过，CI 里没设算失败。
//! 查提交历史用 git 命令行。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use affine_sync_client::{Comment, Error as ClientError, Span, comment_spans, comment_text};
use serde_json::{Value, json};
use tcms_affine::workspace::{self, NodeKind};
use tcms_affine::{
    Affine, Context, Event, FollowEnv, FollowEvent, FollowLink, FollowOptions, Job, Keep,
    RandomIds, Section, SyncReport, follow, link, resolve, split, status, sync, sync_comments,
    to_blocks, to_markdown,
};
use tcms_core::{ItemId, Scope};
use tcms_git::{Author, ContextRepo, FileEdit};
use tcms_store::{Action, Identity, NewProposal, NewToken, Proposed, Store};
use tcms_write::AcceptOptions;
use yrs::updates::decoder::Decode;
use yrs::{Doc, ReadTxn, StateVector, Transact, Update};

const WS: &str = "ws-test";
const A: &str = "ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA";
const B: &str = "ctx_01JDX4M6Q8BBBBBBBBBBBBBBBB";
const C: &str = "ctx_01JDX4M6Q8CCCCCCCCCCCCCCCC";
const D: &str = "ctx_01JDX4M6Q8DDDDDDDDDDDDDDDD";

/// 内存里的假 AFFiNE。
#[derive(Default)]
struct Memory {
    docs: HashMap<String, Doc>,
    pushes: usize,
    /// 评论：文档 id → 这篇上的评论（回复在 `replies` 里）。
    comments: HashMap<String, Vec<Comment>>,
    /// 发出去的评论、回复一共几条（id 用它编）。
    comment_count: usize,
    /// 模拟标成已解决时出错。
    fail_resolve: bool,
    /// 一次取几篇的评论问了几次；模拟一次取几篇时出错。
    batches: usize,
    fail_batch: bool,
    /// 取了几次文档（看全部看一遍时跳过了没变的）。
    loads: Cell<usize>,
}

impl Memory {
    fn doc(&self, id: &str) -> &Doc {
        self.docs
            .get(id)
            .unwrap_or_else(|| panic!("假 AFFiNE 里没有 {id}"))
    }

    /// 根文档里的文档列表。
    fn pages(&self) -> Vec<workspace::DocMeta> {
        workspace::read_meta(self.doc(WS)).pages
    }

    fn page_id(&self, title: &str) -> String {
        self.pages()
            .into_iter()
            .find(|p| p.title == title)
            .unwrap_or_else(|| panic!("没有标题是 {title} 的文档"))
            .id
    }

    /// 一篇文档的条目：（标题块 id，标题，正文 Markdown）。
    fn items(&self, doc_id: &str) -> Vec<(String, String, String)> {
        let page = tcms_affine::read(self.doc(doc_id)).unwrap();
        split(&page.blocks)
            .into_iter()
            .filter_map(|s| match s {
                Section::Item(i) => Some((
                    i.heading_id.clone().unwrap(),
                    i.title.clone(),
                    i.body_markdown(),
                )),
                Section::Loose(_) => None,
            })
            .collect()
    }

    fn heading(&self, doc_id: &str, title: &str) -> String {
        self.items(doc_id)
            .into_iter()
            .find(|(_, t, _)| t == title)
            .unwrap_or_else(|| panic!("{doc_id} 里没有「{title}」"))
            .0
    }

    /// 模拟人在 AFFiNE 里改一条。
    fn human_edit(&self, doc_id: &str, title: &str, new_title: &str, body: &str) {
        let h = self.heading(doc_id, title);
        tcms_affine::update_item(
            self.doc(doc_id),
            &h,
            new_title,
            &to_blocks(body),
            &mut RandomIds,
        )
        .unwrap();
    }
}

impl Memory {
    fn load_now(&self, doc: &str, sv: Option<&[u8]>) -> Option<Vec<u8>> {
        self.loads.set(self.loads.get() + 1);
        let d = self.docs.get(doc)?;
        let sv = match sv {
            Some(b) => StateVector::decode_v1(b).unwrap(),
            None => StateVector::default(),
        };
        Some(d.transact().encode_state_as_update_v1(&sv))
    }

    fn push_now(&mut self, doc: &str, update: &[u8], create: bool) {
        assert_eq!(create, !self.docs.contains_key(doc), "{doc}：新建 {create}");
        self.pushes += 1;
        let d = self.docs.entry(doc.to_owned()).or_default();
        d.transact_mut()
            .apply_update(Update::decode_v1(update).unwrap())
            .unwrap();
    }

    /// 每篇文档“最后改的时间”：用整篇状态的哈希代替，文档一变就变（真 AFFiNE 是毫秒时间戳）。
    /// 不能只用状态向量：只删了东西时状态向量不变。
    fn stamps_now(&self) -> HashMap<String, i64> {
        self.docs
            .iter()
            .map(|(id, d)| {
                let mut h = DefaultHasher::new();
                d.transact()
                    .encode_state_as_update_v1(&StateVector::default())
                    .hash(&mut h);
                (id.clone(), h.finish() as i64)
            })
            .collect()
    }

    fn editors_now(&self) -> HashMap<String, String> {
        self.docs
            .keys()
            .map(|k| (k.clone(), "小王".to_owned()))
            .collect()
    }

    fn new_comment(&mut self, user: &str, content: Value) -> Comment {
        self.comment_count += 1;
        Comment {
            id: format!("comment-{}", self.comment_count),
            content,
            resolved: false,
            user_id: user.to_owned(),
            user_name: user.to_owned(),
            created_at: format!("2026-11-25T00:00:{:02}Z", self.comment_count),
            updated_at: String::new(),
            replies: Vec::new(),
        }
    }

    fn comment_mut(&mut self, id: &str) -> Result<&mut Comment, ClientError> {
        self.comments
            .values_mut()
            .flatten()
            .find(|c| c.id == id)
            .ok_or_else(|| ClientError::Protocol(format!("没有评论 {id}")))
    }

    fn create_comment_now(&mut self, doc: &str, content: Value) -> String {
        let c = self.new_comment("tcms", content);
        let id = c.id.clone();
        self.comments.entry(doc.to_owned()).or_default().push(c);
        id
    }

    fn reply_now(&mut self, comment_id: &str, user: &str, text: &str) -> Result<(), ClientError> {
        let content = comment_spans(&[vec![Span::Text(text)]], None).unwrap();
        let r = self.new_comment(user, content);
        self.comment_mut(comment_id)?.replies.push(r);
        Ok(())
    }

    /// 一篇文档上的评论（只有一条时用）：（评论的字，回复的字，标成已解决了没有）。
    fn comment_on(&self, doc: &str) -> (String, Vec<String>, bool) {
        let list = &self.comments[doc];
        assert_eq!(list.len(), 1, "{list:?}");
        let c = &list[0];
        let replies = c.replies.iter().map(|r| comment_text(&r.content)).collect();
        (comment_text(&c.content), replies, c.resolved)
    }
}

impl Affine for Memory {
    async fn load(
        &mut self,
        _ws: &str,
        doc: &str,
        sv: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, ClientError> {
        Ok(self.load_now(doc, sv))
    }

    async fn push(
        &mut self,
        _ws: &str,
        doc: &str,
        update: &[u8],
        create: bool,
    ) -> Result<(), ClientError> {
        self.push_now(doc, update, create);
        Ok(())
    }

    async fn editors(&mut self, _ws: &str) -> Result<HashMap<String, String>, ClientError> {
        Ok(self.editors_now())
    }

    async fn comments(&mut self, _ws: &str, doc: &str) -> Result<Vec<Comment>, ClientError> {
        Ok(self.comments.get(doc).cloned().unwrap_or_default())
    }

    async fn create_comment(
        &mut self,
        _ws: &str,
        doc: &str,
        _title: &str,
        content: Value,
    ) -> Result<String, ClientError> {
        Ok(self.create_comment_now(doc, content))
    }

    async fn reply(&mut self, id: &str, _title: &str, content: Value) -> Result<(), ClientError> {
        self.reply_now(id, "tcms", &comment_text(&content))
    }

    async fn resolve_comment(&mut self, id: &str) -> Result<(), ClientError> {
        if self.fail_resolve {
            return Err(ClientError::Protocol("假的出错".to_owned()));
        }
        self.comment_mut(id)?.resolved = true;
        Ok(())
    }

    async fn comments_of(
        &mut self,
        _ws: &str,
        docs: &[String],
    ) -> Result<HashMap<String, Vec<Comment>>, ClientError> {
        self.batches += 1;
        if self.fail_batch {
            return Err(ClientError::Protocol("一次取几篇时假的出错".to_owned()));
        }
        Ok(docs
            .iter()
            .map(|d| (d.clone(), self.comments.get(d).cloned().unwrap_or_default()))
            .collect())
    }

    async fn doc_timestamps(
        &mut self,
        _ws: &str,
    ) -> Result<Option<HashMap<String, i64>>, ClientError> {
        Ok(Some(self.stamps_now()))
    }
}

/// 跟着同步的测试用：同步程序和测试脚本用同一个假 AFFiNE；脚本模拟人改了以后发一个广播。
struct Shared {
    mem: Rc<RefCell<Memory>>,
    events: tokio::sync::mpsc::UnboundedReceiver<Event>,
}

impl Affine for Shared {
    async fn load(
        &mut self,
        _ws: &str,
        doc: &str,
        sv: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, ClientError> {
        Ok(self.mem.borrow().load_now(doc, sv))
    }

    async fn push(
        &mut self,
        _ws: &str,
        doc: &str,
        update: &[u8],
        create: bool,
    ) -> Result<(), ClientError> {
        self.mem.borrow_mut().push_now(doc, update, create);
        Ok(())
    }

    async fn editors(&mut self, _ws: &str) -> Result<HashMap<String, String>, ClientError> {
        Ok(self.mem.borrow().editors_now())
    }

    async fn comments(&mut self, _ws: &str, doc: &str) -> Result<Vec<Comment>, ClientError> {
        Ok(self
            .mem
            .borrow()
            .comments
            .get(doc)
            .cloned()
            .unwrap_or_default())
    }

    async fn create_comment(
        &mut self,
        _ws: &str,
        doc: &str,
        _title: &str,
        content: Value,
    ) -> Result<String, ClientError> {
        Ok(self.mem.borrow_mut().create_comment_now(doc, content))
    }

    async fn reply(&mut self, id: &str, _title: &str, content: Value) -> Result<(), ClientError> {
        self.mem
            .borrow_mut()
            .reply_now(id, "tcms", &comment_text(&content))
    }

    async fn resolve_comment(&mut self, id: &str) -> Result<(), ClientError> {
        self.mem.borrow_mut().comment_mut(id)?.resolved = true;
        Ok(())
    }

    async fn doc_timestamps(
        &mut self,
        _ws: &str,
    ) -> Result<Option<HashMap<String, i64>>, ClientError> {
        Ok(Some(self.mem.borrow().stamps_now()))
    }

    async fn next_event(&mut self) -> Event {
        match self.events.recv().await {
            Some(e) => e,
            None => std::future::pending().await,
        }
    }
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
    author: Author,
}

impl Fixture {
    fn cx(&self) -> Context<'_> {
        Context {
            repo: &self.repo,
            store: &self.store,
            workspace: WS,
            actor: "测试",
            remote: None,
            author: &self.author,
        }
    }

    fn path(&self) -> &Path {
        self.repo.workdir().unwrap()
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.path().join(path)).ok()
    }

    /// 人直接改上下文仓库（提交，再索引）。
    async fn git_commit(&self, edits: &[FileEdit]) {
        self.repo
            .commit(edits, "人改的", &Author::default())
            .unwrap();
        tcms_index::reindex(&self.repo, &self.store, false)
            .await
            .unwrap();
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(self.path())
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    async fn sync(&self, affine: &mut Memory, resume: &[&str]) -> SyncReport {
        let resume: Vec<String> = resume.iter().map(|s| (*s).to_owned()).collect();
        sync(&self.cx(), affine, &resume).await.unwrap()
    }
}

const MACRO: &str = "role-macro/数据源.md";

/// 两边都没变（没有二级标题、接不上的文件每次都报一下“没接”）。
fn quiet(r: &SyncReport) -> bool {
    r.docs
        .iter()
        .all(|d| matches!(d.state, "unchanged" | "skipped"))
        && r.commits.is_empty()
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
            put(
                MACRO,
                &format!(
                    "---\nkind: fact\ntags: [数据]\n---\n# 数据源\n\n宏观岗位用的数据。\n\n## CPI 从哪里取\n<!-- ctx id={A} owner=alice -->\n\nCPI 用国家统计局的数。\n\n- 月度\n- 同比\n\n## PMI\n<!-- ctx id={B} -->\n\nPMI 用统计局和财新两个。\n"
                ),
            ),
            put(
                "role-macro/表格.md",
                &format!("## 口径表\n<!-- ctx id={C} -->\n\n| 指标 | 来源 |\n| --- | --- |\n| CPI | 统计局 |\n"),
            ),
            put("role-macro/单条.md", "# 只有一条\n\n没有二级标题的文件。\n"),
        ],
        "第一版",
        &Author::default(),
    )
    .unwrap();
    tcms_index::reindex(&repo, &store, false).await.unwrap();
    Some(Fixture {
        _dir: dir,
        repo,
        store,
        author: Author::default(),
    })
}

/// 接上 role-macro，假 AFFiNE 里先有一个只有名字的工作区。
async fn linked() -> Option<(Fixture, Memory)> {
    let f = fixture().await?;
    let mut affine = Memory::default();
    let root = Doc::new();
    workspace::set_workspace_name(&root, "团队上下文");
    affine.docs.insert(WS.to_owned(), root);
    let scope: Scope = "role:macro".parse().unwrap();
    let r = link(&f.cx(), &mut affine, &scope, None).await.unwrap();
    let states: Vec<(&str, &str)> = r.docs.iter().map(|d| (d.path.as_str(), d.state)).collect();
    assert_eq!(
        states,
        vec![
            ("role-macro/单条.md", "skipped"),
            (MACRO, "linked"),
            ("role-macro/表格.md", "linked"),
        ]
    );
    assert!(r.commits.is_empty());
    Some((f, affine))
}

#[tokio::test]
async fn link_writes_docs_folders_and_bases() {
    let Some((f, affine)) = linked().await else {
        return;
    };
    // 工作区根上一个 role-macro 文件夹，里面链着两篇文档。
    let folders = workspace::folders(affine.doc(&tcms_affine::folders_doc_id(WS)));
    let folder = folders
        .iter()
        .find(|n| n.kind == NodeKind::Folder && n.data == "role-macro")
        .unwrap();
    let doc_id = affine.page_id("数据源");
    let links: Vec<&str> = folders
        .iter()
        .filter(|n| n.parent_id.as_deref() == Some(folder.id.as_str()))
        .map(|n| n.data.as_str())
        .collect();
    assert_eq!(links.len(), 2);
    assert!(links.contains(&doc_id.as_str()));
    // 文档：标题是文件名，开头的一级标题和说明照样有，每条一个二级标题，YAML 和 ctx 注释不进去。
    let page = tcms_affine::read(affine.doc(&doc_id)).unwrap();
    assert_eq!(page.title, "数据源");
    let md = to_markdown(&page.blocks);
    assert!(
        md.starts_with("# 数据源\n\n宏观岗位用的数据。\n\n## CPI 从哪里取"),
        "{md}"
    );
    assert!(!md.contains("ctx") && !md.contains("kind"), "{md}");
    let items = affine.items(&doc_id);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].2, "CPI 用国家统计局的数。\n\n- 月度\n- 同比");

    let s = status(&f.store).await.unwrap();
    assert_eq!(s.scopes.len(), 1);
    assert!(s.scopes[0].whole);
    assert_eq!(s.docs.len(), 2);
    assert_eq!(s.docs.iter().map(|d| d.items).sum::<usize>(), 3);

    // 什么都没变：不推、不提交。
    let mut affine = affine;
    let pushes = affine.pushes;
    let r = f.sync(&mut affine, &[]).await;
    assert!(quiet(&r), "{r:?}");
    assert!(r.commits.is_empty());
    assert_eq!(affine.pushes, pushes);
}

#[tokio::test]
async fn edits_go_both_ways() {
    let Some((f, mut affine)) = linked().await else {
        return;
    };
    let doc_id = affine.page_id("数据源");
    let before = f.read(MACRO).unwrap();

    // 人在 AFFiNE 里改了第一条：写进 git，属性不动，别的地方逐字节不变。
    affine.human_edit(
        &doc_id,
        "CPI 从哪里取",
        "CPI 从哪里取",
        "CPI 用国家统计局的数，**季调后**。",
    );
    let r = f.sync(&mut affine, &[]).await;
    let d = r.docs.iter().find(|d| d.path == MACRO).unwrap();
    assert_eq!(d.state, "synced");
    assert_eq!(d.to_git.updated, 1);
    assert_eq!(r.commits.len(), 1);
    let after = f.read(MACRO).unwrap();
    assert!(
        after.contains("CPI 用国家统计局的数，**季调后**。"),
        "{after}"
    );
    assert!(
        after.contains(&format!(
            "<!-- ctx id={A} kind=fact tags=数据 owner=alice -->"
        )),
        "{after}"
    );
    let tail = |t: &str| t[t.find("## PMI").unwrap()..].to_owned();
    assert_eq!(tail(&after), tail(&before));
    assert!(
        after.starts_with("---\nkind: fact\ntags: [数据]\n---\n# 数据源\n\n宏观岗位用的数据。\n")
    );
    let log = f.git(&["log", "-1", "--format=%an <%ae>%n%B"]);
    assert!(
        log.starts_with("小王 <affine@tcms.invalid>\nAFFiNE：role-macro/数据源.md（改 1 条）"),
        "{log}"
    );
    assert!(
        log.contains(&format!("Item: {A}")) && log.contains("TCMS-Sync: affine"),
        "{log}"
    );
    // 索引跟着更新了。
    let row = f.store.get_item(A).await.unwrap().unwrap();
    assert_eq!(row.body, "CPI 用国家统计局的数，**季调后**。");
    assert_eq!(row.revision, 2);

    // 再同步一次：自己写的不会再写回去。
    let r = f.sync(&mut affine, &[]).await;
    assert!(quiet(&r), "{r:?}");

    // 人改了 git 里的第二条：写进 AFFiNE，不提交。
    let text = f
        .read(MACRO)
        .unwrap()
        .replace("PMI 用统计局和财新两个。", "PMI 只用统计局的。");
    f.git_commit(&[put(MACRO, &text)]).await;
    let r = f.sync(&mut affine, &[]).await;
    let d = r.docs.iter().find(|d| d.path == MACRO).unwrap();
    assert_eq!(
        (d.state, d.to_affine.updated, d.to_git.total()),
        ("synced", 1, 0)
    );
    assert!(r.commits.is_empty());
    let items = affine.items(&doc_id);
    assert_eq!(items[1].2, "PMI 只用统计局的。");
    let r = f.sync(&mut affine, &[]).await;
    assert!(quiet(&r), "{r:?}");
}

#[tokio::test]
async fn full_sync_skips_unchanged_docs() {
    let Some((f, mut affine)) = linked().await else {
        return;
    };
    let doc_id = affine.page_id("数据源");
    let table_id = affine.page_id("表格");
    const TABLE: &str = "role-macro/表格.md";
    let stamp = |store: &Store, path: &'static str| {
        let store = store.clone();
        async move {
            store
                .affine_docs()
                .await
                .unwrap()
                .into_iter()
                .find(|d| d.path == path)
                .unwrap()
                .affine_ts
        }
    };

    // 接上时推过两篇，还没记 AFFiNE 说的时间：第一次全部看一遍每篇都取，两边都没变，记下来。
    assert_eq!(stamp(&f.store, MACRO).await, None);
    let r = f.sync(&mut affine, &[]).await;
    assert!(quiet(&r), "{r:?}");
    let now = affine.stamps_now();
    assert_eq!(stamp(&f.store, MACRO).await, Some(now[&doc_id]));
    assert_eq!(stamp(&f.store, TABLE).await, Some(now[&table_id]));

    // 再看一遍：两篇都不取，只取工作区根文档和文件夹树。
    affine.loads.set(0);
    let r = f.sync(&mut affine, &[]).await;
    assert!(quiet(&r), "{r:?}");
    assert_eq!(affine.loads.get(), 2);

    // 人在 AFFiNE 里改了一篇：只取这篇。存了同步结果，记的时间清掉，下次看一遍再取一次、再记下。
    affine.human_edit(&doc_id, "PMI", "PMI", "PMI 只用财新的。");
    affine.loads.set(0);
    let r = f.sync(&mut affine, &[]).await;
    let state = |r: &SyncReport, p: &str| r.docs.iter().find(|d| d.path == p).unwrap().state;
    assert_eq!(
        (state(&r, MACRO), state(&r, TABLE)),
        ("synced", "unchanged")
    );
    assert_eq!(affine.loads.get(), 3);
    assert!(f.read(MACRO).unwrap().contains("PMI 只用财新的。"));
    assert_eq!(stamp(&f.store, MACRO).await, None);
    let r = f.sync(&mut affine, &[]).await;
    assert!(quiet(&r), "{r:?}");
    assert_eq!(
        stamp(&f.store, MACRO).await,
        Some(affine.stamps_now()[&doc_id])
    );

    // 人改了 git 里的另一篇：AFFiNE 说的时间没变也要取，改动推过去。
    let text = f
        .read(TABLE)
        .unwrap()
        .replace("| CPI | 统计局 |", "| CPI | 统计局、Wind |");
    f.git_commit(&[put(TABLE, &text)]).await;
    affine.loads.set(0);
    let r = f.sync(&mut affine, &[]).await;
    assert_eq!(
        (state(&r, MACRO), state(&r, TABLE)),
        ("unchanged", "synced")
    );
    assert_eq!(affine.loads.get(), 3);
    assert!(affine.items(&table_id)[0].2.contains("统计局、Wind"));
    let r = f.sync(&mut affine, &[]).await;
    assert!(quiet(&r), "{r:?}");
    affine.loads.set(0);
    let r = f.sync(&mut affine, &[]).await;
    assert!(quiet(&r), "{r:?}");
    assert_eq!(affine.loads.get(), 2);
}

#[tokio::test]
async fn new_items_and_deletions() {
    let Some((f, mut affine)) = linked().await else {
        return;
    };
    let doc_id = affine.page_id("数据源");
    // AFFiNE 里在第一条后面加一条：git 里放在第一条后面，类型用文件开头的默认值，有新 id。
    let h = affine.heading(&doc_id, "CPI 从哪里取");
    tcms_affine::insert_item(
        affine.doc(&doc_id),
        Some(&h),
        "PPI",
        &to_blocks("PPI 也用统计局的。"),
        &mut RandomIds,
    )
    .unwrap();
    // git 里在最后加一条。
    let text = f.read(MACRO).unwrap();
    let d_id = ItemId::generate();
    f.git_commit(&[put(
        MACRO,
        &format!("{text}\n## 社融\n<!-- ctx id={d_id} -->\n\n社融看人民银行。\n"),
    )])
    .await;
    let r = f.sync(&mut affine, &[]).await;
    let d = r.docs.iter().find(|d| d.path == MACRO).unwrap();
    assert_eq!((d.to_git.added, d.to_affine.added), (1, 1), "{d:?}");
    let text = f.read(MACRO).unwrap();
    let (cpi, ppi, pmi) = (
        text.find("## CPI").unwrap(),
        text.find("## PPI").unwrap(),
        text.find("## PMI").unwrap(),
    );
    assert!(cpi < ppi && ppi < pmi, "{text}");
    let ppi_row = f
        .store
        .all_items()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.title == "PPI")
        .unwrap();
    assert_eq!(ppi_row.kind, "fact");
    let titles: Vec<String> = affine.items(&doc_id).into_iter().map(|i| i.1).collect();
    assert_eq!(titles, ["CPI 从哪里取", "PPI", "PMI", "社融"]);
    let r = f.sync(&mut affine, &[]).await;
    assert!(quiet(&r), "{r:?}");

    // AFFiNE 里删掉一条（4 条里的 1 条）：git 里归档。
    tcms_affine::remove_item(affine.doc(&doc_id), &affine.heading(&doc_id, "PMI")).unwrap();
    let r = f.sync(&mut affine, &[]).await;
    let d = r.docs.iter().find(|d| d.path == MACRO).unwrap();
    assert_eq!(
        d.to_git.removed,
        1,
        "{d:?} {:?}",
        status(&f.store).await.unwrap()
    );
    assert!(!f.read(MACRO).unwrap().contains("## PMI"));
    assert_eq!(
        f.store.get_item(B).await.unwrap().unwrap().status,
        "archived"
    );

    // 一次删掉一半以上：停下来，确认以后才归档。
    for t in ["PPI", "社融"] {
        tcms_affine::remove_item(affine.doc(&doc_id), &affine.heading(&doc_id, t)).unwrap();
    }
    let r = f.sync(&mut affine, &[]).await;
    let d = r.docs.iter().find(|d| d.path == MACRO).unwrap();
    assert_eq!(d.state, "paused");
    assert!(r.needs_attention());
    assert!(f.read(MACRO).unwrap().contains("## PPI"));
    let r = f.sync(&mut affine, &[]).await;
    assert_eq!(
        r.docs.iter().find(|d| d.path == MACRO).unwrap().state,
        "paused"
    );
    let r = f.sync(&mut affine, &[MACRO]).await;
    let d = r.docs.iter().find(|d| d.path == MACRO).unwrap();
    assert_eq!((d.state, d.to_git.removed), ("synced", 2));
    let text = f.read(MACRO).unwrap();
    assert!(
        !text.contains("## PPI") && !text.contains("## 社融") && text.contains("## CPI"),
        "{text}"
    );
    let s = status(&f.store).await.unwrap();
    assert!(s.docs.iter().all(|d| d.paused.is_none()));
}

#[tokio::test]
async fn both_sides_changed_is_held_until_resolved() {
    let Some((f, mut affine)) = linked().await else {
        return;
    };
    let doc_id = affine.page_id("数据源");
    affine.human_edit(&doc_id, "PMI", "PMI", "AFFiNE 里写的。");
    let text = f
        .read(MACRO)
        .unwrap()
        .replace("PMI 用统计局和财新两个。", "git 里写的。");
    f.git_commit(&[put(MACRO, &text)]).await;
    let r = f.sync(&mut affine, &[]).await;
    let d = r.docs.iter().find(|d| d.path == MACRO).unwrap();
    assert_eq!(d.held, 1);
    assert!(r.needs_attention());
    let s = status(&f.store).await.unwrap();
    assert_eq!(s.conflicts.len(), 1);
    let c = &s.conflicts[0];
    assert_eq!(c.reason, "both");
    assert_eq!(c.git_body.as_deref(), Some("git 里写的。"));
    assert_eq!(c.affine_body.as_deref(), Some("AFFiNE 里写的。"));
    assert_eq!(c.base_body.as_deref(), Some("PMI 用统计局和财新两个。"));

    // 没处理之前，两边都不动这一条，别的条目照常同步。
    affine.human_edit(&doc_id, "CPI 从哪里取", "CPI 从哪里取", "CPI 改了。");
    let r = f.sync(&mut affine, &[]).await;
    let d = r.docs.iter().find(|d| d.path == MACRO).unwrap();
    assert_eq!((d.to_git.updated, d.to_affine.total(), d.held), (1, 0, 0));
    assert!(f.read(MACRO).unwrap().contains("git 里写的。"));
    assert_eq!(affine.items(&doc_id)[1].2, "AFFiNE 里写的。");

    // 留 AFFiNE 的：写进 git。
    resolve(&f.store, c.id, Keep::Affine, "测试").await.unwrap();
    let r = f.sync(&mut affine, &[]).await;
    let d = r.docs.iter().find(|d| d.path == MACRO).unwrap();
    assert_eq!((d.to_git.updated, d.to_affine.total()), (1, 0), "{d:?}");
    assert!(f.read(MACRO).unwrap().contains("AFFiNE 里写的。"));
    assert!(status(&f.store).await.unwrap().conflicts.is_empty());

    // 再来一次，这次留 git 的：写进 AFFiNE。
    affine.human_edit(&doc_id, "PMI", "PMI", "AFFiNE 第二次。");
    let text = f
        .read(MACRO)
        .unwrap()
        .replace("AFFiNE 里写的。", "git 第二次。");
    f.git_commit(&[put(MACRO, &text)]).await;
    f.sync(&mut affine, &[]).await;
    let id = status(&f.store).await.unwrap().conflicts[0].id;
    resolve(&f.store, id, Keep::Git, "测试").await.unwrap();
    let r = f.sync(&mut affine, &[]).await;
    let d = r.docs.iter().find(|d| d.path == MACRO).unwrap();
    assert_eq!((d.to_git.total(), d.to_affine.updated), (0, 1), "{d:?}");
    assert_eq!(affine.items(&doc_id)[1].2, "git 第二次。");
    let r = f.sync(&mut affine, &[]).await;
    assert!(quiet(&r), "{r:?}");
}

#[tokio::test]
async fn readonly_items_and_whole_scope() {
    let Some((f, mut affine)) = linked().await else {
        return;
    };
    // 表格在 AFFiNE 里只能看：那边改了不往回写，记下来。
    let table_doc = affine.page_id("表格");
    affine.human_edit(&table_doc, "口径表", "口径表", "改坏了的表");
    let r = f.sync(&mut affine, &[]).await;
    let d = r
        .docs
        .iter()
        .find(|d| d.path == "role-macro/表格.md")
        .unwrap();
    assert_eq!((d.held, d.to_git.total()), (1, 0));
    assert_eq!(
        status(&f.store).await.unwrap().conflicts[0].reason,
        "readonly"
    );
    assert!(
        f.read("role-macro/表格.md")
            .unwrap()
            .contains("| CPI | 统计局 |")
    );

    // 整个范围接上了：git 里新加的文件建文档，AFFiNE 文件夹里新建的文档建文件。
    let new_id = ItemId::generate();
    f.git_commit(&[put(
        "role-macro/新的.md",
        &format!("## 新文件里的一条\n<!-- ctx id={new_id} -->\n\n正文。\n"),
    )])
    .await;
    let folder = workspace::folders(affine.doc(&tcms_affine::folders_doc_id(WS)))
        .into_iter()
        .find(|n| n.kind == NodeKind::Folder)
        .unwrap();
    let human_doc = Doc::new();
    tcms_affine::create(
        &human_doc,
        "AFFiNE 里写的",
        &to_blocks("开头一段。\n\n## 第一条\n\n内容一\n\n## 第二条\n\n内容二"),
        &mut RandomIds,
    )
    .unwrap();
    affine.docs.insert("humanDoc01".to_owned(), human_doc);
    workspace::upsert_doc_meta(
        affine.doc(WS),
        &workspace::DocMeta::new("humanDoc01", "AFFiNE 里写的", 1.7e12),
    );
    workspace::create_link(
        affine.doc(&tcms_affine::folders_doc_id(WS)),
        &folder.id,
        NodeKind::Doc,
        "humanDoc01",
        &mut RandomIds,
    )
    .unwrap();
    let r = f.sync(&mut affine, &[]).await;
    let states: HashMap<&str, &str> = r.docs.iter().map(|d| (d.path.as_str(), d.state)).collect();
    assert_eq!(states.get("role-macro/新的.md"), Some(&"linked"), "{r:?}");
    assert_eq!(
        states.get("role-macro/AFFiNE 里写的.md"),
        Some(&"file_created"),
        "{r:?}"
    );
    let text = f.read("role-macro/AFFiNE 里写的.md").unwrap();
    assert!(text.starts_with("开头一段。\n"), "{text}");
    assert!(
        text.contains("## 第一条\n<!-- ctx id=ctx_") && text.contains("内容二"),
        "{text}"
    );
    let titles: Vec<String> = affine
        .items(&affine.page_id("新的"))
        .into_iter()
        .map(|i| i.1)
        .collect();
    assert_eq!(titles, ["新文件里的一条"]);
    let r = f.sync(&mut affine, &[]).await;
    assert!(quiet(&r), "{r:?}");

    // git 里删了一个文件、AFFiNE 里也没改过：文档移进回收站，不再同步。
    f.git_commit(&[FileEdit {
        path: "role-macro/新的.md".to_owned(),
        content: None,
    }])
    .await;
    let r = f.sync(&mut affine, &[]).await;
    let d = r
        .docs
        .iter()
        .find(|d| d.path == "role-macro/新的.md")
        .unwrap();
    assert_eq!(d.state, "unlinked");
    let page = affine
        .pages()
        .into_iter()
        .find(|p| p.title == "新的")
        .unwrap();
    assert!(page.trash);
    assert!(
        status(&f.store)
            .await
            .unwrap()
            .docs
            .iter()
            .all(|d| d.path != "role-macro/新的.md")
    );
}

/// 发一个 agent 的令牌，换成它背后的身份。
async fn agent(store: &Store) -> Identity {
    let (_, secret) = store
        .create_token(
            &NewToken {
                name: "宏观研究员".to_owned(),
                role: "macro".to_owned(),
                team: Some("research".to_owned()),
                projects: Vec::new(),
                expires_in_days: None,
            },
            "测试",
        )
        .await
        .unwrap();
    store.identify(&secret).await.unwrap().unwrap()
}

/// 直接存一条提议（不走检查），返回 id。正文和证据不该出现在评论里。
async fn proposal(
    f: &Fixture,
    who: &Identity,
    action: Action,
    item: Option<&str>,
    title: &str,
    reason: &str,
) -> String {
    let base_revision = match item {
        Some(id) => Some(f.store.get_item(id).await.unwrap().unwrap().revision),
        None => None,
    };
    let p = NewProposal {
        action,
        item_id: item.map(str::to_owned),
        base_revision,
        scope: "role:macro".to_owned(),
        kind: "fact".to_owned(),
        title: title.to_owned(),
        body: "提议的正文不进评论。".to_owned(),
        tags: Vec::new(),
        review_by: None,
        reason: reason.to_owned(),
        evidence: vec!["https://chat.example.com/对话/1".to_owned()],
        conflicts: json!([]),
        content_hash: format!("{action}{item:?}{title}"),
        redacted: 0,
    };
    match f.store.create_proposal(who, &p, 50).await.unwrap() {
        Proposed::New(row) => row.id,
        other => panic!("没建成：{other:?}"),
    }
}

fn accept_options() -> AcceptOptions {
    AcceptOptions {
        actor: "王审核（tok_01TEST）".to_owned(),
        author: Author::default(),
        title: None,
        body: None,
        file: None,
        over_revision: None,
        note: None,
        remote: None,
    }
}

const REVIEW: &str = "https://ctx.example.com/";

/// 提议显示成评论：目标接上了的发评论（不放正文、证据），发过的不再发；人在 AFFiNE 里的回复记下来；
/// 批准、拒绝以后回复结果、标成已解决；目标后来才接上的，接上以后再发；评论在 AFFiNE 里删了的不再管。
#[tokio::test]
async fn proposals_show_as_comments() {
    let Some((f, mut affine)) = linked().await else {
        return;
    };
    let macro_doc = affine.page_id("数据源");
    let table_doc = affine.page_id("表格");
    let who = agent(&f.store).await;
    let long_reason = "统计局从 2026 年起公布季调后的 CPI。".repeat(20);
    let update = proposal(
        &f,
        &who,
        Action::Update,
        Some(A),
        "CPI 从哪里取（季调）",
        &long_reason,
    )
    .await;
    let archive = proposal(&f, &who, Action::Archive, Some(C), "口径表", "表格过时了").await;
    // 新建的默认写进 role-macro/事实.md，这个文件还没有。
    let create = proposal(&f, &who, Action::Create, None, "PPI 的口径", "新加一条").await;

    let r = sync_comments(&f.store, &mut affine, WS, Some(REVIEW))
        .await
        .unwrap();
    assert_eq!(r.posted, [update.clone(), archive.clone()], "{r:?}");
    assert!(r.errors.is_empty(), "{r:?}");
    let (text, replies, resolved) = affine.comment_on(&macro_doc);
    let reason_start: String = long_reason.chars().take(200).collect();
    assert_eq!(
        text,
        format!(
            "宏观研究员（岗位 macro）提议修改「CPI 从哪里取」，标题改成「CPI 从哪里取（季调）」。\n理由：{reason_start}…\n批准、拒绝在审核页：打开这条提议。在这里回复的，审核页里看得到。"
        )
    );
    assert!(replies.is_empty() && !resolved);
    let raw = serde_json::to_string(&affine.comments[&macro_doc][0].content).unwrap();
    assert!(raw.contains(&format!(
        "https://ctx.example.com/review#/proposals/{update}"
    )));
    assert!(raw.contains(r#""preview":"CPI 从哪里取""#), "{raw}");
    assert!(!raw.contains("正文不进评论") && !raw.contains("chat.example.com"));
    let (text, _, _) = affine.comment_on(&table_doc);
    assert!(text.starts_with("宏观研究员（岗位 macro）提议归档「口径表」。\n理由：表格过时了\n"));
    let row = f.store.affine_comment(&update).await.unwrap().unwrap();
    assert_eq!(
        (row.doc_id.as_str(), row.path.as_str()),
        (macro_doc.as_str(), MACRO)
    );
    assert!(!row.closed);

    // 再看一遍：什么都不用做。
    let r = sync_comments(&f.store, &mut affine, WS, Some(REVIEW))
        .await
        .unwrap();
    assert!(r.is_empty(), "{r:?}");

    // 人在 AFFiNE 里回复了：记下来。
    let comment = row.comment_id.clone();
    affine
        .reply_now(&comment, "小李", "季调的数从哪里下？")
        .unwrap();
    let r = sync_comments(&f.store, &mut affine, WS, Some(REVIEW))
        .await
        .unwrap();
    assert_eq!(r.replies, std::slice::from_ref(&update), "{r:?}");
    let row = f.store.affine_comment(&update).await.unwrap().unwrap();
    assert_eq!(row.replies.as_array().unwrap().len(), 1);
    assert_eq!(row.replies[0]["user"], "小李");
    assert_eq!(row.replies[0]["text"], "季调的数从哪里下？");

    // 拒绝一条、批准一条：回复结果，标成已解决。
    tcms_write::reject(&f.store, &update, "王审核（tok_01TEST）", Some("先不改"))
        .await
        .unwrap();
    let accepted = tcms_write::accept(&f.repo, &f.store, &archive, &accept_options())
        .await
        .unwrap();
    let r = sync_comments(&f.store, &mut affine, WS, Some(REVIEW))
        .await
        .unwrap();
    let mut closed = r.closed.clone();
    closed.sort();
    let mut want = vec![update.clone(), archive.clone()];
    want.sort();
    assert_eq!(closed, want, "{r:?}");
    let (_, replies, resolved) = affine.comment_on(&macro_doc);
    assert_eq!(replies, ["季调的数从哪里下？", "已拒绝（王审核）：先不改"]);
    assert!(resolved);
    let (_, replies, resolved) = affine.comment_on(&table_doc);
    let commit = &accepted.commit[..12];
    assert_eq!(
        replies,
        [format!(
            "已接受（王审核），写进了上下文仓库，提交 {commit}。"
        )]
    );
    assert!(resolved);
    // 自己回复的结果不算人的回复。
    let row = f.store.affine_comment(&update).await.unwrap().unwrap();
    assert!(row.closed);
    assert_eq!(row.replies.as_array().unwrap().len(), 1);

    // 新建的目标文件后来有了（整个范围接上了，同步时接上）：接上以后发评论。
    f.git_commit(&[put(
        "role-macro/事实.md",
        &format!("## 别的事实\n<!-- ctx id={D} -->\n\n一条。\n"),
    )])
    .await;
    f.sync(&mut affine, &[]).await;
    let fact_doc = affine.page_id("事实");
    let r = sync_comments(&f.store, &mut affine, WS, None)
        .await
        .unwrap();
    assert_eq!(r.posted, std::slice::from_ref(&create), "{r:?}");
    let (text, _, _) = affine.comment_on(&fact_doc);
    assert_eq!(
        text,
        format!(
            "宏观研究员（岗位 macro）提议在这篇里新加一条「PPI 的口径」。\n理由：新加一条\n批准、拒绝在审核页的“待审核的提议”里（{create}）。在这里回复的，审核页里看得到。"
        )
    );

    // 人在 AFFiNE 里把评论删了：不再管，也不再发。
    affine.comments.get_mut(&fact_doc).unwrap().clear();
    let r = sync_comments(&f.store, &mut affine, WS, None)
        .await
        .unwrap();
    assert_eq!(r.gone, std::slice::from_ref(&create), "{r:?}");
    let r = sync_comments(&f.store, &mut affine, WS, None)
        .await
        .unwrap();
    assert!(r.is_empty(), "{r:?}");
    assert!(affine.comments[&fact_doc].is_empty());
}

/// 回复了结果、标成已解决时出错：记下错，下次只标成已解决，不再回复一遍。
#[tokio::test]
async fn comment_outcomes_are_said_once() {
    let Some((f, mut affine)) = linked().await else {
        return;
    };
    let macro_doc = affine.page_id("数据源");
    let who = agent(&f.store).await;
    let id = proposal(&f, &who, Action::Update, Some(B), "PMI", "只用统计局的").await;
    let r = sync_comments(&f.store, &mut affine, WS, None)
        .await
        .unwrap();
    assert_eq!(r.posted, std::slice::from_ref(&id));
    tcms_write::reject(&f.store, &id, "cli", None)
        .await
        .unwrap();

    affine.fail_resolve = true;
    let r = sync_comments(&f.store, &mut affine, WS, None)
        .await
        .unwrap();
    assert!(r.closed.is_empty(), "{r:?}");
    assert_eq!(r.errors.len(), 1, "{r:?}");
    assert!(r.errors[0].contains("假的出错"), "{r:?}");
    let (_, replies, resolved) = affine.comment_on(&macro_doc);
    assert_eq!(replies, ["已拒绝（cli）。"]);
    assert!(!resolved);

    affine.fail_resolve = false;
    let r = sync_comments(&f.store, &mut affine, WS, None)
        .await
        .unwrap();
    assert_eq!(r.closed, std::slice::from_ref(&id), "{r:?}");
    let (_, replies, resolved) = affine.comment_on(&macro_doc);
    assert_eq!(replies, ["已拒绝（cli）。"]);
    assert!(resolved);
    let row = f.store.affine_comment(&id).await.unwrap().unwrap();
    assert!(row.closed);
    assert_eq!(row.replies, json!([]));
}

/// 发过的评论一次取齐；一次取出错时一篇一篇取，照样处理。
#[tokio::test]
async fn comments_fall_back_one_by_one() {
    let Some((f, mut affine)) = linked().await else {
        return;
    };
    let macro_doc = affine.page_id("数据源");
    let who = agent(&f.store).await;
    let id = proposal(&f, &who, Action::Update, Some(B), "PMI", "只用统计局的").await;
    let r = sync_comments(&f.store, &mut affine, WS, None)
        .await
        .unwrap();
    assert_eq!(r.posted, std::slice::from_ref(&id));
    // 刚发的评论还没取过：没有发过、还没关的，不用问。
    assert_eq!(affine.batches, 0);
    let r = sync_comments(&f.store, &mut affine, WS, None)
        .await
        .unwrap();
    assert!(r.is_empty(), "{r:?}");
    assert_eq!(affine.batches, 1);

    tcms_write::reject(&f.store, &id, "cli", None)
        .await
        .unwrap();
    affine.fail_batch = true;
    let r = sync_comments(&f.store, &mut affine, WS, None)
        .await
        .unwrap();
    assert_eq!(affine.batches, 2);
    assert_eq!(r.closed, std::slice::from_ref(&id), "{r:?}");
    assert!(r.errors.is_empty(), "{r:?}");
    let (_, replies, resolved) = affine.comment_on(&macro_doc);
    assert_eq!(replies, ["已拒绝（cli）。"]);
    assert!(resolved);
}

/// AFFiNE 0.27.5 网页版的客户端 id 超过 32 位（2026-11-24 在网页编辑器里打字，看到的是 50 位的）。
/// 这段更新是 yjs 13.6.21 用客户端 id 710053731018286 写的：插进“## 标题”，再删掉“## ”（网页里打 `## ` 变成标题就是这样）。
/// yrs 开了 small-client 时块里的 id 和删除集合里的 id 截得不一样，读出来还是“## 标题”。
#[test]
fn deletions_by_53_bit_clients_are_read() {
    const UPDATE: &[u8] = &[
        1, 2, 174, 204, 162, 163, 164, 185, 161, 1, 0, 1, 1, 1, 116, 3, 132, 174, 204, 162, 163,
        164, 185, 161, 1, 2, 6, 230, 160, 135, 233, 162, 152, 1, 174, 204, 162, 163, 164, 185, 161,
        1, 1, 0, 3,
    ];
    let doc = Doc::new();
    doc.transact_mut()
        .apply_update(Update::decode_v1(UPDATE).unwrap())
        .unwrap();
    let text = doc.get_or_insert_text("t");
    let txn = doc.transact();
    assert_eq!(yrs::GetString::get_string(&text, &txn), "标题");
    assert!(txn.store().pending_ds().is_none());
    // 再编码一次，id 不变。
    let again = txn.encode_state_as_update_v1(&StateVector::default());
    assert_eq!(again, UPDATE);
}

/// 跟着同步的日志：每次同步一行（`full:` 或 `docs:`，后面是有变化的文件），出错一行。
fn describe(ev: &FollowEvent<'_>) -> String {
    match ev {
        FollowEvent::Synced(job, r) => {
            let changed: Vec<String> = r
                .docs
                .iter()
                .filter(|d| !matches!(d.state, "unchanged" | "skipped"))
                .map(|d| format!("{} {}", d.path, d.state))
                .collect();
            let kind = match job {
                Job::Full => "full",
                Job::Docs(_) => "docs",
            };
            format!("{kind}:{}", changed.join(","))
        }
        FollowEvent::Comments(r) => format!(
            "comments:posted {} closed {} gone {} replies {} errors {}",
            r.posted.len(),
            r.closed.len(),
            r.gone.len(),
            r.replies.len(),
            r.errors.len()
        ),
        FollowEvent::Failed(e) => format!("failed:{e}"),
        FollowEvent::Disconnected(r) => format!("disconnected:{r}"),
        FollowEvent::Reconnected => "reconnected".to_owned(),
    }
}

/// 等到 `ok()`，最多 10 秒。
async fn wait_for(what: &str, ok: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(tokio::time::Instant::now() < deadline, "等不到：{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn follow_syncs_both_ways_without_holding_the_lock() {
    let Some((f, affine)) = linked().await else {
        return;
    };
    let doc_id = affine.page_id("数据源");
    let mem = Rc::new(RefCell::new(affine));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut shared = Shared {
        mem: mem.clone(),
        events: rx,
    };
    let env = FollowEnv {
        repo: f.path().to_path_buf(),
        store: f.store.clone(),
        workspace: WS.to_owned(),
        actor: "测试".to_owned(),
        remote: None,
        author: Author::default(),
        link: FollowLink::default(),
        public_url: None,
    };
    let ms = Duration::from_millis;
    let opts = FollowOptions {
        quiet: ms(200),
        max_wait: ms(2000),
        git_poll: ms(200),
        full_every: Duration::from_secs(600),
        retry: ms(200),
    };
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let log: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let changed = |doc: &str| Event::Changed {
        workspace: WS.to_owned(),
        doc: doc.to_owned(),
    };

    let script = async {
        // 刚开始全部看一遍：两边都没变。
        wait_for("第一次同步", || !log.borrow().is_empty()).await;
        assert_eq!(*log.borrow(), ["full:"]);

        // 人在 AFFiNE 里改了第一条，推来广播：停一下以后写进 git。
        mem.borrow().human_edit(
            &doc_id,
            "CPI 从哪里取",
            "CPI 从哪里取",
            "CPI 用国家统计局的数，**季调后**。",
        );
        tx.send(changed(&doc_id)).unwrap();
        wait_for("AFFiNE 里的修改写进 git", || log.borrow().len() >= 2).await;
        assert_eq!(log.borrow()[1], format!("docs:{MACRO} synced"));
        assert!(f.read(MACRO).unwrap().contains("**季调后**"));
        let log_line = f.git(&["log", "-1", "--format=%an%n%B"]);
        assert!(log_line.starts_with("小王\nAFFiNE："), "{log_line}");

        // 人改了 git 里的第二条（提交、索引）：不用等，写进 AFFiNE。
        let text = f
            .read(MACRO)
            .unwrap()
            .replace("PMI 用统计局和财新两个。", "PMI 只用统计局的。");
        f.git_commit(&[put(MACRO, &text)]).await;
        wait_for("git 里的修改写进 AFFiNE", || {
            mem.borrow().items(&doc_id)[1].2 == "PMI 只用统计局的。"
        })
        .await;
        wait_for("同步完", || log.borrow().len() >= 3).await;
        assert_eq!(log.borrow()[2], format!("docs:{MACRO} synced"));

        // 两边都没变的不拿提交队列的锁：别处拿着锁的时候来了广播，照样很快看完。
        let lock = f.store.begin_commit().await.unwrap();
        tx.send(changed(&doc_id)).unwrap();
        wait_for("拿着锁的时候看完", || log.borrow().len() >= 4).await;
        assert_eq!(log.borrow()[3], "docs:");
        drop(lock);

        // git 里新加一个文件（整个范围接上了）：接上，在 AFFiNE 里建文档。
        f.git_commit(&[put(
            "role-macro/新的.md",
            &format!("## 新条目\n<!-- ctx id={D} -->\n\n新的一条。\n"),
        )])
        .await;
        wait_for("新文件接上", || {
            mem.borrow().pages().iter().any(|p| p.title == "新的")
        })
        .await;
        wait_for("同步完", || log.borrow().len() >= 5).await;
        assert_eq!(log.borrow()[4], "docs:role-macro/新的.md linked");
        stop.send(true).unwrap();
    };
    let follower = follow(&env, &mut shared, opts, stopped, |ev| {
        log.borrow_mut().push(describe(&ev))
    });
    tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(follower, script);
    })
    .await
    .expect("跟着同步没停下来");
    assert_eq!(log.borrow().len(), 5, "{:?}", log.borrow());
    let s = status(&f.store).await.unwrap();
    assert_eq!(s.docs.len(), 3);
}

/// 审核页通过 `FollowLink` 看跟着同步的状态，处理完等人处理的、让停着的文档接着做以后让它马上同步。
#[tokio::test]
async fn follow_takes_requests_from_the_review_page() {
    let Some((f, affine)) = linked().await else {
        return;
    };
    let doc_id = affine.page_id("数据源");
    let table_doc = affine.page_id("表格");
    // 开始跟着同步之前：PMI 两边都改了；表格那篇在 AFFiNE 里删掉了唯一的一条（一半以上，会停下来）。
    affine.human_edit(&doc_id, "PMI", "PMI", "AFFiNE 里写的。");
    let text = f
        .read(MACRO)
        .unwrap()
        .replace("PMI 用统计局和财新两个。", "git 里写的。");
    f.git_commit(&[put(MACRO, &text)]).await;
    tcms_affine::remove_item(
        affine.doc(&table_doc),
        &affine.heading(&table_doc, "口径表"),
    )
    .unwrap();

    let mem = Rc::new(RefCell::new(affine));
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut shared = Shared { mem, events: rx };
    let link = FollowLink::default();
    let env = FollowEnv {
        repo: f.path().to_path_buf(),
        store: f.store.clone(),
        workspace: WS.to_owned(),
        actor: "测试".to_owned(),
        remote: None,
        author: Author::default(),
        link: link.clone(),
        public_url: None,
    };
    // 不看 git、不定时全部看：只有第一次和审核页叫的时候同步。
    let opts = FollowOptions {
        quiet: Duration::from_millis(200),
        max_wait: Duration::from_secs(2),
        git_poll: Duration::from_secs(600),
        full_every: Duration::from_secs(600),
        retry: Duration::from_millis(200),
    };
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let log: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let table = "role-macro/表格.md";

    let script = async {
        wait_for("第一次同步", || !log.borrow().is_empty()).await;
        let st = link.state();
        assert_eq!(st.connection, tcms_affine::Connection::Connected);
        assert!(st.since_ms.is_some() && st.last_sync_ms.is_some());
        assert!(st.error.is_none(), "{st:?}");
        let s = status(&f.store).await.unwrap();
        assert_eq!(s.conflicts.len(), 1);
        let paused = s.docs.iter().find(|d| d.path == table).unwrap();
        assert!(paused.paused.is_some(), "{s:?}");

        // 审核页里留了 AFFiNE 的：不用等广播、不用等全部看一遍，马上写进 git。
        resolve(&f.store, s.conflicts[0].id, Keep::Affine, "审核的人")
            .await
            .unwrap();
        link.sync_now(&doc_id);
        wait_for("马上同步那一篇", || log.borrow().len() >= 2).await;
        assert_eq!(log.borrow()[1], format!("docs:{MACRO} synced"));
        assert!(f.read(MACRO).unwrap().contains("AFFiNE 里写的。"));
        assert!(link.queued().docs.is_empty());

        // 审核页里让表格那篇接着做：git 里归档那一条，同步成了才从排着的里去掉。
        link.resume(table);
        assert_eq!(link.queued().resume, [table]);
        wait_for("接着做", || log.borrow().len() >= 3).await;
        assert_eq!(log.borrow()[2], format!("docs:{table} synced"));
        // 唯一的一条归档了，文件也不在了。
        assert!(f.read(table).is_none_or(|t| !t.contains("## 口径表")));
        assert!(link.queued().resume.is_empty());
        let s = status(&f.store).await.unwrap();
        assert!(s.docs.iter().all(|d| d.paused.is_none()), "{s:?}");
        assert!(s.conflicts.is_empty());
        assert!(link.state().last_change_ms.is_some());
        stop.send(true).unwrap();
    };
    let follower = follow(&env, &mut shared, opts, stopped, |ev| {
        log.borrow_mut().push(describe(&ev))
    });
    tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(follower, script);
    })
    .await
    .expect("跟着同步没停下来");
    assert_eq!(log.borrow().len(), 3, "{:?}", log.borrow());
}

/// 跟着同步时，提议一存进来（`NOTIFY tcms_proposals`）几秒内发评论，拒绝了几秒内回复结果、标成已解决；
/// 不用等每 5 分钟全部看一遍。
#[tokio::test]
async fn follow_posts_comments_soon_after_proposals() {
    let Some((f, affine)) = linked().await else {
        return;
    };
    let doc_id = affine.page_id("数据源");
    let mem = Rc::new(RefCell::new(affine));
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut shared = Shared {
        mem: mem.clone(),
        events: rx,
    };
    let env = FollowEnv {
        repo: f.path().to_path_buf(),
        store: f.store.clone(),
        workspace: WS.to_owned(),
        actor: "测试".to_owned(),
        remote: None,
        author: Author::default(),
        link: FollowLink::default(),
        public_url: Some("https://ctx.example.com".to_owned()),
    };
    let opts = FollowOptions {
        quiet: Duration::from_millis(200),
        max_wait: Duration::from_secs(2),
        git_poll: Duration::from_secs(600),
        full_every: Duration::from_secs(600),
        retry: Duration::from_millis(200),
    };
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let log: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let comment_on = |doc: &str| mem.borrow().comments.get(doc).map(|l| l.len()).unwrap_or(0);

    let script = async {
        wait_for("第一次同步", || !log.borrow().is_empty()).await;
        let who = agent(&f.store).await;
        let id = proposal(&f, &who, Action::Update, Some(B), "PMI", "只用统计局的").await;
        wait_for("发评论", || comment_on(&doc_id) == 1).await;
        let (text, _, _) = mem.borrow().comment_on(&doc_id);
        assert!(
            text.starts_with("宏观研究员（岗位 macro）提议修改「PMI」。"),
            "{text}"
        );
        let raw = serde_json::to_string(&mem.borrow().comments[&doc_id][0].content).unwrap();
        assert!(raw.contains(&format!("https://ctx.example.com/review#/proposals/{id}")));

        tcms_write::reject(&f.store, &id, "王审核（tok_01TEST）", Some("再看看"))
            .await
            .unwrap();
        wait_for("回复结果、标成已解决", || {
            mem.borrow().comment_on(&doc_id).2
        })
        .await;
        let (_, replies, _) = mem.borrow().comment_on(&doc_id);
        assert_eq!(replies, ["已拒绝（王审核）：再看看"]);
        stop.send(true).unwrap();
    };
    let follower = follow(&env, &mut shared, opts, stopped, |ev| {
        log.borrow_mut().push(describe(&ev))
    });
    tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(follower, script);
    })
    .await
    .expect("跟着同步没停下来");
    assert_eq!(
        *log.borrow(),
        [
            "full:",
            "comments:posted 1 closed 0 gone 0 replies 0 errors 0",
            "comments:posted 0 closed 1 gone 0 replies 0 errors 0",
        ]
    );
}
