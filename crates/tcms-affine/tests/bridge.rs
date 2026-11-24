//! 同步一次：连真的 PostgreSQL、一个临时的 git 仓库，AFFiNE 换成内存里的假 AFFiNE（只存 Yjs 文档，收发更新）。
//! 人在 AFFiNE 里的修改用 `affine-doc` 直接改假 AFFiNE 里的文档来模拟。
//!
//! 需要环境变量 `TCMS_TEST_DATABASE_URL`，规则和别的集成测试一样：本地没设就跳过，CI 里没设算失败。
//! 查提交历史用 git 命令行。

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use affine_sync_client::Error as ClientError;
use tcms_affine::workspace::{self, NodeKind};
use tcms_affine::{
    Affine, Context, Keep, RandomIds, Section, SyncReport, link, resolve, split, status, sync,
    to_blocks, to_markdown,
};
use tcms_core::{ItemId, Scope};
use tcms_git::{Author, ContextRepo, FileEdit};
use tcms_store::Store;
use yrs::updates::decoder::Decode;
use yrs::{Doc, ReadTxn, StateVector, Transact, Update};

const WS: &str = "ws-test";
const A: &str = "ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA";
const B: &str = "ctx_01JDX4M6Q8BBBBBBBBBBBBBBBB";
const C: &str = "ctx_01JDX4M6Q8CCCCCCCCCCCCCCCC";

/// 内存里的假 AFFiNE。
#[derive(Default)]
struct Memory {
    docs: HashMap<String, Doc>,
    pushes: usize,
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

impl Affine for Memory {
    async fn load(
        &mut self,
        _ws: &str,
        doc: &str,
        sv: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, ClientError> {
        let Some(d) = self.docs.get(doc) else {
            return Ok(None);
        };
        let sv = match sv {
            Some(b) => StateVector::decode_v1(b).unwrap(),
            None => StateVector::default(),
        };
        Ok(Some(d.transact().encode_state_as_update_v1(&sv)))
    }

    async fn push(
        &mut self,
        _ws: &str,
        doc: &str,
        update: &[u8],
        create: bool,
    ) -> Result<(), ClientError> {
        assert_eq!(create, !self.docs.contains_key(doc), "{doc}：新建 {create}");
        self.pushes += 1;
        let d = self.docs.entry(doc.to_owned()).or_default();
        d.transact_mut()
            .apply_update(Update::decode_v1(update).unwrap())
            .unwrap();
        Ok(())
    }

    async fn editors(&mut self, _ws: &str) -> Result<HashMap<String, String>, ClientError> {
        Ok(self
            .docs
            .keys()
            .map(|k| (k.clone(), "小王".to_owned()))
            .collect())
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
    resolve(&f.store, c.id, Keep::Affine).await.unwrap();
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
    resolve(&f.store, id, Keep::Git).await.unwrap();
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
