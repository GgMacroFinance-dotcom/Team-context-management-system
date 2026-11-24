//! 接上、同步一次、看状态、处理冲突（`docs/affine-sync.md` 5.2–5.4、7.3）。
//!
//! 每次都在提交队列里做（[`tcms_write::begin_batch`]）：配了远端时先拉，照着 HEAD 读文件，
//! 连 AFFiNE 取文档（只取上次以后的差量），每篇文档按 [`crate::plan`] 三方比较，写到两边，存下新的底稿。
//! 一篇文档在 git 里的改动做成一个提交，作者写 AFFiNE 里最后改它的人，提交信息带 `TCMS-Sync: affine`。

use std::collections::{BTreeMap, HashMap, HashSet};

use affine_doc::workspace::{self, DocMeta, FolderError, NodeKind};
use affine_doc::{
    Block, Content, ParagraphKind, RandomIds, Section, Span, same_content, split, to_blocks,
    to_markdown,
};
use serde::Serialize;
use tcms_core::edit::{Edit, EditError, ItemText, apply};
use tcms_core::{
    FileDefaults, ItemId, Kind, ParsedItem, Scope, normalize_body, normalize_title, parse_file,
    scope_of_path,
};
use tcms_git::{Author, ContextRepo, FileEdit, ObjectId, Remote};
use tcms_store::{AffineBase, AffineConflict, AffineDoc, AffineSave, AffineScope, Store};
use tcms_write::{Batch, RemoteReport, WriteError, begin_batch};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, ReadTxn, Snapshot, StateVector, Transact, Update};

use crate::plan::{self, AffineItem, BaseChange, GitItem, Held, Pending, Reason, ToAffine, ToGit};
use crate::server::Affine;

/// AFFiNE 写进 git 的提交用的邮箱：不把 AFFiNE 账号的邮箱写进 git。
pub const AFFINE_EMAIL: &str = "affine@tcms.invalid";
/// 同步程序做的提交末尾带的一行。
pub const SYNC_TRAILER: &str = "TCMS-Sync: affine";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("AFFiNE：{0}")]
    Affine(#[from] affine_sync_client::Error),
    #[error(transparent)]
    Write(#[from] WriteError),
    #[error(transparent)]
    Store(#[from] tcms_store::Error),
    #[error(transparent)]
    Git(#[from] tcms_git::Error),
    #[error("AFFiNE 文档：{0}")]
    Doc(#[from] affine_doc::Error),
    #[error("AFFiNE 文件夹：{0}")]
    Folder(#[from] FolderError),
    #[error("{0}")]
    Invalid(String),
}

/// 同步用的上下文。
pub struct Context<'a> {
    pub repo: &'a ContextRepo,
    pub store: &'a Store,
    /// AFFiNE 工作区 id。
    pub workspace: &'a str,
    /// 审计里记的“谁”。
    pub actor: &'a str,
    /// 上下文仓库的远端，`None` 是没配。
    pub remote: Option<&'a Remote>,
    /// 从远端拉的时候合并提交的作者。
    pub author: &'a Author,
}

/// 一边改了几条。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub updated: usize,
    pub added: usize,
    pub removed: usize,
}

impl Counts {
    pub fn total(&self) -> usize {
        self.updated + self.added + self.removed
    }
}

/// 一篇文档这次怎么样了。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DocReport {
    pub path: String,
    pub doc_id: Option<String>,
    /// `linked`（这次接上，新建了文档）、`file_created`（AFFiNE 里新建的文档，这次在 git 里建了文件）、
    /// `synced`（两边有改动，写过去了）、`unchanged`、`paused`（停着，等人看）、
    /// `unlinked`（git 里删了这个文件，文档移进了 AFFiNE 的回收站）、`skipped`（没接上，原因在 notes）。
    pub state: &'static str,
    pub to_git: Counts,
    pub to_affine: Counts,
    /// 这次新记下、等人处理的条数。
    pub held: usize,
    pub commit: Option<String>,
    pub paused: Option<String>,
    pub notes: Vec<String>,
}

impl DocReport {
    fn new(path: &str, doc_id: Option<&str>, state: &'static str) -> Self {
        Self {
            path: path.to_owned(),
            doc_id: doc_id.map(str::to_owned),
            state,
            ..Self::default()
        }
    }
}

/// 一次接上或同步的结果。
#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncReport {
    pub docs: Vec<DocReport>,
    /// 配了远端时，拉和推的结果。
    pub remote: Option<RemoteReport>,
    pub commits: Vec<String>,
}

impl SyncReport {
    /// 有没有停着的文档或等人处理的条目（命令行据此给非 0 退出码）。
    pub fn needs_attention(&self) -> bool {
        self.docs.iter().any(|d| d.state == "paused" || d.held > 0)
    }
}

// ---------------------------------------------------------------- 工作区：根文档和文件夹树

struct Workspace {
    id: String,
    root: Doc,
    root_mark: Snapshot,
    root_new: bool,
    folders: Doc,
    folders_mark: Snapshot,
    folders_new: bool,
}

fn apply_update(doc: &Doc, update: &[u8]) -> Result<(), Error> {
    let u = Update::decode_v1(update)
        .map_err(|e| Error::Invalid(format!("AFFiNE 给的更新解不开：{e}")))?;
    doc.transact_mut()
        .apply_update(u)
        .map_err(|e| Error::Invalid(format!("AFFiNE 给的更新用不上：{e}")))
}

fn full_state(doc: &Doc) -> Vec<u8> {
    doc.transact()
        .encode_state_as_update_v1(&StateVector::default())
}

/// 状态向量加上删掉了什么：只删不加时状态向量不变，要连删除的集合一起比。
fn mark(doc: &Doc) -> Snapshot {
    doc.transact().snapshot()
}

/// 从 `since` 以后的改动（照着它的状态向量编，删除的集合整个带上）；没改时为空。
fn delta(doc: &Doc, since: &Snapshot) -> Option<Vec<u8>> {
    let txn = doc.transact();
    (txn.snapshot() != *since).then(|| txn.encode_state_as_update_v1(&since.state_map))
}

/// 工作区里文件夹树那篇文档在服务端的 id：`db$<工作区 id>$folders`（AFFiNE 前端也这样拼；
/// 只写 `db$folders` 服务端不给读写）。
pub fn folders_doc_id(workspace: &str) -> String {
    format!("db${workspace}$folders")
}

async fn load_full(affine: &mut impl Affine, ws: &str, id: &str) -> Result<(Doc, bool), Error> {
    let doc = Doc::new();
    let found = affine.load(ws, id, None).await?;
    if let Some(u) = &found {
        apply_update(&doc, u)?;
    }
    Ok((doc, found.is_none()))
}

impl Workspace {
    async fn load(affine: &mut impl Affine, ws: &str) -> Result<Self, Error> {
        let (root, root_new) = load_full(affine, ws, ws).await?;
        let (folders, folders_new) = load_full(affine, ws, &folders_doc_id(ws)).await?;
        Ok(Self {
            id: ws.to_owned(),
            root_mark: mark(&root),
            root,
            root_new,
            folders_mark: mark(&folders),
            folders,
            folders_new,
        })
    }

    /// 根文档、文件夹树有改动就推上去。
    async fn push(&mut self, affine: &mut impl Affine) -> Result<(), Error> {
        if let Some(d) = delta(&self.root, &self.root_mark) {
            affine.push(&self.id, &self.id, &d, self.root_new).await?;
            self.root_mark = mark(&self.root);
            self.root_new = false;
        }
        let table = folders_doc_id(&self.id);
        if let Some(d) = delta(&self.folders, &self.folders_mark) {
            affine.push(&self.id, &table, &d, self.folders_new).await?;
            self.folders_mark = mark(&self.folders);
            self.folders_new = false;
        }
        Ok(())
    }

    /// 范围对应的文件夹：记下的那个还在就用它，不在（第一次接，或者人在 AFFiNE 里删了）就在根上新建一个。
    fn folder_for(
        &self,
        scope: &Scope,
        known: Option<&AffineScope>,
    ) -> Result<(String, bool), Error> {
        if let Some(k) = known
            && workspace::folders(&self.folders)
                .iter()
                .any(|n| n.id == k.folder_id && n.kind == NodeKind::Folder)
        {
            return Ok((k.folder_id.clone(), false));
        }
        let id = workspace::create_folder(&self.folders, None, &scope.dir(), &mut RandomIds)?;
        Ok((id, true))
    }

    fn page(&self, doc_id: &str) -> Option<DocMeta> {
        workspace::read_meta(&self.root)
            .pages
            .into_iter()
            .find(|p| p.id == doc_id)
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

/// 新文档的 id：21 个字母、数字、`_`、`-`，和 AFFiNE 自己生成的样子差不多。
fn new_doc_id() -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
    let mut buf = [0u8; 21];
    if getrandom::fill(&mut buf).is_err() {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (t >> ((i * 6) % 120)) as u8 ^ (i as u8);
        }
    }
    buf.iter()
        .map(|b| ALPHABET[usize::from(b & 63)] as char)
        .collect()
}

// ---------------------------------------------------------------- git 这一边

/// 文件名（不带目录和 `.md`），新文档的标题用它。
fn file_stem(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".md").unwrap_or(name).to_owned()
}

fn is_h2(b: &Block) -> bool {
    matches!(
        b.content,
        Content::Paragraph {
            kind: ParagraphKind::Heading(2),
            ..
        }
    )
}

fn lines_of(text: &str) -> Vec<&str> {
    text.strip_prefix('\u{feff}')
        .unwrap_or(text)
        .split_inclusive('\n')
        .collect()
}

/// 文件里有没有二级标题（没有的话整个文件是一条，AFFiNE 里对不上，先不接）。
fn has_h2(text: &str, body_start: usize) -> bool {
    let lines = lines_of(text);
    to_blocks(&lines[body_start.min(lines.len())..].concat())
        .iter()
        .any(is_h2)
}

/// 一个文件的条目（有 id 的），和没有 id 的条数。
fn git_items(items: &[ParsedItem]) -> (Vec<GitItem>, usize) {
    let mut out = Vec::new();
    let mut missing = 0;
    for it in items {
        match &it.id {
            Some(id) => out.push(GitItem::new(id.as_str(), &it.title, &it.body)),
            None => missing += 1,
        }
    }
    (out, missing)
}

fn default_kind(defaults: &FileDefaults) -> Kind {
    defaults
        .kind
        .as_deref()
        .and_then(|k| k.parse().ok())
        .unwrap_or(Kind::Reference)
}

/// AFFiNE 里新来的一条写进 git 时的样子：属性用文件开头的默认值。
fn new_item_text(
    id: &str,
    title: &str,
    body: &str,
    defaults: &FileDefaults,
) -> Result<ItemText, String> {
    Ok(ItemText {
        id: id.parse()?,
        kind: default_kind(defaults),
        title: normalize_title(title),
        body: normalize_body(body),
        tags: defaults.tags.clone().unwrap_or_default(),
        owner: defaults.owner.clone(),
        review_by: defaults.review_by.clone(),
        supersedes: None,
    })
}

fn commit_message(path: &str, counts: &Counts, items: &[String], editor: &str) -> String {
    let mut parts = Vec::new();
    if counts.updated > 0 {
        parts.push(format!("改 {} 条", counts.updated));
    }
    if counts.added > 0 {
        parts.push(format!("加 {} 条", counts.added));
    }
    if counts.removed > 0 {
        parts.push(format!("归档 {} 条", counts.removed));
    }
    let subject: String = format!("AFFiNE：{path}（{}）", parts.join("、"))
        .chars()
        .take(72)
        .collect();
    let mut msg = format!("{subject}\n\n");
    for id in items {
        msg.push_str(&format!("Item: {id}\n"));
    }
    msg.push_str(&format!("Edited-by: {editor}（AFFiNE）\n{SYNC_TRAILER}\n"));
    msg
}

fn affine_author(name: Option<&String>) -> Author {
    Author {
        name: name.cloned().unwrap_or_else(|| "AFFiNE".to_owned()),
        email: AFFINE_EMAIL.to_owned(),
    }
}

fn blob_at(
    repo: &ContextRepo,
    head: Option<ObjectId>,
    path: &str,
) -> Result<Option<String>, Error> {
    let Some(head) = head else { return Ok(None) };
    Ok(repo.files(head)?.get(path).map(ToString::to_string))
}

// ---------------------------------------------------------------- AFFiNE 这一边

/// 文档里的条目（标题块 id、标题、正文）。
fn affine_items(doc: &Doc) -> Result<Vec<AffineItem>, Error> {
    let page = affine_doc::read(doc)?;
    Ok(split(&page.blocks)
        .into_iter()
        .filter_map(|s| match s {
            Section::Item(i) => Some(AffineItem {
                heading_id: i.heading_id.clone()?,
                body: i.body_markdown(),
                title: i.title,
                blocks: i.body,
            }),
            Section::Loose(_) => None,
        })
        .collect())
}

fn held_row(path: &str, h: &Held) -> AffineConflict {
    AffineConflict {
        id: 0,
        path: path.to_owned(),
        item_id: h.item_id.clone(),
        heading_id: h.heading_id.clone(),
        reason: h.reason.as_str().to_owned(),
        message: h.message.clone(),
        git_title: h.git.as_ref().map(|t| t.title.clone()),
        git_body: h.git.as_ref().map(|t| t.body.clone()),
        affine_title: h.affine.as_ref().map(|t| t.title.clone()),
        affine_body: h.affine.as_ref().map(|t| t.body.clone()),
        base_title: h.base.as_ref().map(|t| t.title.clone()),
        base_body: h.base.as_ref().map(|t| t.body.clone()),
        found_at: String::new(),
    }
}

fn invalid(item_id: Option<&str>, heading_id: Option<&str>, message: String) -> Held {
    Held {
        reason: Reason::Invalid,
        item_id: item_id.map(str::to_owned),
        heading_id: heading_id.map(str::to_owned),
        git: None,
        affine: None,
        base: None,
        message: Some(message),
    }
}

// ---------------------------------------------------------------- 接上

/// 接上一个范围（`file` 为空时整个范围，以后范围里新加的文件、文件夹里新建的文档也同步），
/// 或者范围里的一个文件。已经接上的文件不动。
pub async fn link(
    cx: &Context<'_>,
    affine: &mut impl Affine,
    scope: &Scope,
    file: Option<&str>,
) -> Result<SyncReport, Error> {
    if let Some(f) = file
        && scope_of_path(f).as_ref() != Some(scope)
    {
        return Err(Error::Invalid(format!(
            "{f} 不是范围 {scope} 里的条目文件（第一层目录要是 {}，扩展名 .md）",
            scope.dir()
        )));
    }
    let batch = begin_batch(cx.repo, cx.store, cx.remote, cx.author).await?;
    let mut ws = Workspace::load(affine, cx.workspace).await?;
    let known = cx
        .store
        .affine_scopes()
        .await?
        .into_iter()
        .find(|s| s.scope == scope.to_string());
    if let Some(k) = &known
        && k.workspace_id != cx.workspace
    {
        return Err(Error::Invalid(format!(
            "范围 {scope} 已经接在另一个 AFFiNE 工作区 {} 上了",
            k.workspace_id
        )));
    }
    let row = ensure_scope(cx, affine, &mut ws, scope, known.as_ref(), file.is_none()).await?;
    let linked: HashSet<String> = cx
        .store
        .affine_docs()
        .await?
        .into_iter()
        .map(|d| d.path)
        .collect();
    let files = files_in(cx.repo, batch.head(), scope)?;
    let wanted: Vec<String> = match file {
        Some(f) if !files.contains(f) => {
            return Err(Error::Invalid(format!("上下文仓库里没有 {f}")));
        }
        Some(f) => vec![f.to_owned()],
        None => files.into_iter().collect(),
    };
    let mut report = SyncReport::default();
    for path in wanted {
        if linked.contains(&path) {
            continue;
        }
        report
            .docs
            .push(link_file(cx, affine, &mut ws, &batch, &row, &path).await?);
    }
    finish(cx, batch, &mut report, "affine.link", &scope.to_string()).await?;
    Ok(report)
}

async fn ensure_scope(
    cx: &Context<'_>,
    affine: &mut impl Affine,
    ws: &mut Workspace,
    scope: &Scope,
    known: Option<&AffineScope>,
    whole: bool,
) -> Result<AffineScope, Error> {
    let (folder_id, created) = ws.folder_for(scope, known)?;
    if created {
        ws.push(affine).await?;
    }
    let row = AffineScope {
        scope: scope.to_string(),
        workspace_id: cx.workspace.to_owned(),
        folder_id,
        whole: whole || known.is_some_and(|k| k.whole),
    };
    if known != Some(&row) {
        cx.store.put_affine_scope(&row).await?;
    }
    Ok(row)
}

/// 范围里的条目文件（照着 `head`）。
fn files_in(
    repo: &ContextRepo,
    head: Option<ObjectId>,
    scope: &Scope,
) -> Result<std::collections::BTreeSet<String>, Error> {
    let Some(head) = head else {
        return Ok(Default::default());
    };
    Ok(repo
        .files(head)?
        .into_keys()
        .filter(|p| scope_of_path(p).as_ref() == Some(scope))
        .collect())
}

/// 在 AFFiNE 里给一个文件新建文档：文件开头（第一个二级标题前面，去掉 YAML）照样放进去，每条一个二级标题块。
async fn link_file(
    cx: &Context<'_>,
    affine: &mut impl Affine,
    ws: &mut Workspace,
    batch: &Batch,
    scope: &AffineScope,
    path: &str,
) -> Result<DocReport, Error> {
    let Some(text) = batch.read(cx.repo, path)? else {
        return Ok(DocReport::new(path, None, "skipped"));
    };
    let parsed = parse_file(path, &text);
    if !has_h2(&text, parsed.body_start) || parsed.items.is_empty() {
        let mut r = DocReport::new(path, None, "skipped");
        r.notes
            .push("没有二级标题：整个文件是一条，AFFiNE 里对不上，先不接".to_owned());
        return Ok(r);
    }
    let lines = lines_of(&text);
    let first = parsed.items[0].span.0.min(lines.len());
    let mut blocks = to_blocks(&lines[parsed.body_start.min(first)..first].concat());
    let (items, missing) = git_items(&parsed.items);
    for it in &items {
        blocks.push(Block::paragraph(
            ParagraphKind::Heading(2),
            vec![Span::plain(&it.title)],
        ));
        blocks.extend(to_blocks(&it.body));
    }
    let doc = Doc::new();
    let title = file_stem(path);
    affine_doc::create(&doc, &title, &blocks, &mut RandomIds)?;
    let written = affine_items(&doc)?;
    if written.len() != items.len() {
        return Err(Error::Invalid(format!(
            "{path} 写进 AFFiNE 以后读回来是 {} 条，应该是 {} 条",
            written.len(),
            items.len()
        )));
    }
    let mut bases = Vec::new();
    for (w, it) in written.iter().zip(&items) {
        if w.title != it.title || !same_content(&w.blocks, &to_blocks(&it.body)) {
            return Err(Error::Invalid(format!(
                "{path} 里的「{}」写进 AFFiNE 以后读回来对不上",
                it.title
            )));
        }
        bases.push(AffineBase {
            item_id: it.id.clone(),
            path: path.to_owned(),
            heading_id: w.heading_id.clone(),
            title: it.title.clone(),
            body: it.body.clone(),
        });
    }
    let doc_id = new_doc_id();
    let state = full_state(&doc);
    affine.push(&ws.id, &doc_id, &state, true).await?;
    workspace::upsert_doc_meta(&ws.root, &DocMeta::new(&doc_id, &title, now_ms()));
    workspace::create_link(
        &ws.folders,
        &scope.folder_id,
        NodeKind::Doc,
        &doc_id,
        &mut RandomIds,
    )?;
    ws.push(affine).await?;
    cx.store
        .save_affine_doc(&AffineSave {
            doc: AffineDoc {
                path: path.to_owned(),
                scope: scope.scope.clone(),
                workspace_id: ws.id.clone(),
                doc_id: doc_id.clone(),
                state,
                git_blob: blob_at(cx.repo, batch.head(), path)?,
                synced_commit: batch.head().map(|h| h.to_string()),
                paused: None,
                synced_at: String::new(),
            },
            set: bases,
            remove: Vec::new(),
            conflicts: Vec::new(),
        })
        .await?;
    let mut r = DocReport::new(path, Some(&doc_id), "linked");
    r.to_affine.added = items.len();
    if missing > 0 {
        r.notes.push(format!(
            "{missing} 条还没有 id，没放进 AFFiNE；补上 id（tcms assign-ids）以后下次同步放进去"
        ));
    }
    Ok(r)
}

async fn finish(
    cx: &Context<'_>,
    batch: Batch,
    report: &mut SyncReport,
    action: &str,
    target: &str,
) -> Result<(), Error> {
    report.commits = batch.commits().to_vec();
    let detail = serde_json::json!({
        "commits": report.commits,
        "docs": report.docs.iter().filter(|d| d.state != "unchanged").map(|d| serde_json::json!({
            "path": d.path, "state": d.state, "to_git": d.to_git, "to_affine": d.to_affine, "held": d.held,
        })).collect::<Vec<_>>(),
    });
    report.remote = batch
        .finish(cx.repo, cx.store, cx.actor, action, target, detail)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------- 同步一次

/// 这一次同步里大家共用的。
struct Run<'r> {
    ws: Workspace,
    batch: Batch,
    bases: &'r [AffineBase],
    conflicts: &'r [AffineConflict],
    resume: &'r [String],
    /// 文档 id → 最后改的人；要往 git 写的时候才去问。
    editors: Option<HashMap<String, String>>,
}

impl Run<'_> {
    async fn editor(&mut self, affine: &mut impl Affine, doc_id: &str) -> Author {
        if self.editors.is_none() {
            // 拿不到名字不影响同步，提交的作者写“AFFiNE”。
            self.editors = Some(affine.editors(&self.ws.id).await.unwrap_or_default());
        }
        affine_author(self.editors.as_ref().and_then(|m| m.get(doc_id)))
    }
}

/// 同步一次：整个接上的范围里新加的文件放进 AFFiNE、文件夹里新建的文档写成文件，再把每篇接上的文档三方比较、写到两边。
/// `resume` 里的文件停过（一次要归档一半以上的条目、AFFiNE 里移进了回收站、写完读回来对不上），人看过了，这次接着做。
pub async fn sync(
    cx: &Context<'_>,
    affine: &mut impl Affine,
    resume: &[String],
) -> Result<SyncReport, Error> {
    let batch = begin_batch(cx.repo, cx.store, cx.remote, cx.author).await?;
    let ws = Workspace::load(affine, cx.workspace).await?;
    let scopes = cx.store.affine_scopes().await?;
    let docs = cx.store.affine_docs().await?;
    let bases = cx.store.affine_bases().await?;
    let conflicts = cx.store.affine_conflicts().await?;
    let mut run = Run {
        ws,
        batch,
        bases: &bases,
        conflicts: &conflicts,
        resume,
        editors: None,
    };
    let mut report = SyncReport::default();
    let mut known_paths: HashSet<String> = docs.iter().map(|d| d.path.clone()).collect();
    let mut known_docs: HashSet<String> = docs.iter().map(|d| d.doc_id.clone()).collect();

    for s in scopes.iter().filter(|s| s.workspace_id == cx.workspace) {
        let Ok(scope) = s.scope.parse::<Scope>() else {
            continue;
        };
        let (folder_id, created) = run.ws.folder_for(&scope, Some(s))?;
        let row = if created {
            run.ws.push(affine).await?;
            let row = AffineScope {
                folder_id,
                ..s.clone()
            };
            cx.store.put_affine_scope(&row).await?;
            row
        } else {
            s.clone()
        };
        if !row.whole {
            continue;
        }
        // git 里新加的文件。
        for path in files_in(cx.repo, run.batch.head(), &scope)? {
            if known_paths.insert(path.clone()) {
                let r = link_file(cx, affine, &mut run.ws, &run.batch, &row, &path).await?;
                known_docs.extend(r.doc_id.clone());
                report.docs.push(r);
            }
        }
        // AFFiNE 里这个文件夹下新建的文档。
        let new_docs: Vec<String> = workspace::folders(&run.ws.folders)
            .into_iter()
            .filter(|n| {
                n.parent_id.as_deref() == Some(row.folder_id.as_str())
                    && n.kind == NodeKind::Doc
                    && !known_docs.contains(&n.data)
            })
            .map(|n| n.data)
            .collect();
        for doc_id in new_docs {
            if run.ws.page(&doc_id).is_none_or(|p| p.trash) {
                continue;
            }
            let r = file_from_doc(
                cx,
                affine,
                &mut run,
                &row,
                &scope,
                &doc_id,
                &mut known_paths,
            )
            .await?;
            report.docs.push(r);
        }
    }

    for d in docs.iter().filter(|d| d.workspace_id == cx.workspace) {
        let r = sync_doc(cx, affine, &mut run, d).await?;
        report.docs.push(r);
    }
    run.ws.push(affine).await?;
    finish(cx, run.batch, &mut report, "affine.sync", cx.workspace).await?;
    Ok(report)
}

async fn sync_doc(
    cx: &Context<'_>,
    affine: &mut impl Affine,
    run: &mut Run<'_>,
    d: &AffineDoc,
) -> Result<DocReport, Error> {
    let path = d.path.as_str();
    let resumed = run.resume.iter().any(|p| p == path);
    let mut report = DocReport::new(path, Some(&d.doc_id), "unchanged");
    if let Some(why) = &d.paused
        && !resumed
    {
        report.state = "paused";
        report.paused = Some(why.clone());
        return Ok(report);
    }

    // AFFiNE 这边：从上次的状态起，只取差量。
    let local = Doc::new();
    apply_update(&local, &d.state)?;
    let before_load = mark(&local);
    let loaded = affine
        .load(
            &run.ws.id,
            &d.doc_id,
            Some(&before_load.state_map.encode_v1()),
        )
        .await?;
    if let Some(u) = &loaded {
        apply_update(&local, u)?;
    }
    let affine_moved = mark(&local) != before_load;
    let deleted_in_affine = loaded.is_none() || run.ws.page(&d.doc_id).is_some_and(|p| p.trash);

    // git 这边。
    let head = run.batch.head();
    let blob = blob_at(cx.repo, head, path)?;
    if !resumed && blob == d.git_blob && !affine_moved && !deleted_in_affine {
        return Ok(report);
    }
    if deleted_in_affine && !resumed {
        let why = format!(
            "AFFiNE 里这篇文档移进了回收站（或删掉了）。要在 git 里也归档整篇的话：tcms affine sync --resume {path}；不要的话在 AFFiNE 里把它恢复"
        );
        cx.store.set_affine_paused(path, Some(&why)).await?;
        report.state = "paused";
        report.paused = Some(why);
        return Ok(report);
    }
    let text = match &blob {
        Some(_) => run.batch.read(cx.repo, path)?,
        None => None,
    };
    let parsed = text.as_deref().map(|t| parse_file(path, t));
    let (git, missing) = parsed
        .as_ref()
        .map(|p| git_items(&p.items))
        .unwrap_or_default();
    if missing > 0 {
        report.notes.push(format!(
            "{missing} 条还没有 id，这次不同步；补上 id（tcms assign-ids）以后下次放进 AFFiNE"
        ));
    }
    let affine_now = if deleted_in_affine {
        Vec::new()
    } else {
        affine_items(&local)?
    };
    let doc_bases: Vec<plan::Base> = run
        .bases
        .iter()
        .filter(|b| b.path == path)
        .map(|b| plan::Base {
            item_id: b.item_id.clone(),
            heading_id: b.heading_id.clone(),
            title: b.title.clone(),
            body: b.body.clone(),
        })
        .collect();
    let mut pending = Pending::default();
    for c in run.conflicts.iter().filter(|c| c.path == path) {
        pending.item_ids.extend(c.item_id.clone());
        pending.heading_ids.extend(c.heading_id.clone());
    }
    let mut new_id = || ItemId::generate().to_string();
    let plan = plan::plan(&git, &affine_now, &doc_bases, &pending, &mut new_id);

    // git 里删了这个文件，AFFiNE 里也没改过：文档移进回收站，不再同步。
    if text.is_none() && plan.held.is_empty() && !deleted_in_affine {
        if let Some(mut page) = run.ws.page(&d.doc_id) {
            page.trash = true;
            page.updated_date = Some(now_ms());
            page.extra
                .insert("trashDate".to_owned(), yrs::Any::from(now_ms()));
            workspace::upsert_doc_meta(&run.ws.root, &page);
        }
        cx.store.remove_affine_doc(path).await?;
        report.state = "unlinked";
        return Ok(report);
    }
    let archived = plan.archived();
    if archived > 0 && archived * 2 > doc_bases.len() && !resumed {
        let why = format!(
            "AFFiNE 里一次删掉了 {} 条里的 {archived} 条，没有在 git 里归档。确认要归档的话：tcms affine sync --resume {path}；不要的话在 AFFiNE 里撤销",
            doc_bases.len()
        );
        cx.store.set_affine_paused(path, Some(&why)).await?;
        report.state = "paused";
        report.paused = Some(why);
        return Ok(report);
    }

    let mut held = plan.held.clone();
    let mut failed_items: HashSet<String> = HashSet::new();
    let mut failed_headings: HashSet<String> = HashSet::new();
    let mut extra_bases: Vec<AffineBase> = Vec::new();
    let mut paused: Option<String> = None;

    // 写 AFFiNE。文档已经在回收站里（人确认了要归档）就不写了。
    if !deleted_in_affine {
        let before = mark(&local);
        let mut heading_of: HashMap<String, String> = doc_bases
            .iter()
            .map(|b| (b.item_id.clone(), b.heading_id.clone()))
            .collect();
        let item_of = |h: &str| {
            doc_bases
                .iter()
                .find(|b| b.heading_id == h)
                .map(|b| b.item_id.clone())
        };
        let mut written: Vec<(String, String, String)> = Vec::new();
        for op in &plan.to_affine {
            match op {
                ToAffine::Update {
                    heading_id,
                    title,
                    body,
                } => match affine_doc::update_item(
                    &local,
                    heading_id,
                    title,
                    &to_blocks(body),
                    &mut RandomIds,
                ) {
                    Ok(_) => {
                        report.to_affine.updated += 1;
                        written.push((heading_id.clone(), title.clone(), body.clone()));
                    }
                    Err(e) => {
                        let item = item_of(heading_id);
                        failed_headings.insert(heading_id.clone());
                        held.push(invalid(
                            item.as_deref(),
                            Some(heading_id),
                            format!("写不进 AFFiNE：{e}"),
                        ));
                    }
                },
                ToAffine::Remove { heading_id } => {
                    match affine_doc::remove_item(&local, heading_id) {
                        Ok(()) => report.to_affine.removed += 1,
                        Err(e) => {
                            let item = item_of(heading_id);
                            failed_headings.insert(heading_id.clone());
                            held.push(invalid(
                                item.as_deref(),
                                Some(heading_id),
                                format!("AFFiNE 里删不掉：{e}"),
                            ));
                        }
                    }
                }
                ToAffine::Insert {
                    item_id,
                    after,
                    title,
                    body,
                } => {
                    let after_heading = after.as_ref().and_then(|a| heading_of.get(a)).cloned();
                    match affine_doc::insert_item(
                        &local,
                        after_heading.as_deref(),
                        title,
                        &to_blocks(body),
                        &mut RandomIds,
                    ) {
                        Ok((h, _)) => {
                            report.to_affine.added += 1;
                            heading_of.insert(item_id.clone(), h.clone());
                            written.push((h.clone(), title.clone(), body.clone()));
                            extra_bases.push(AffineBase {
                                item_id: item_id.clone(),
                                path: path.to_owned(),
                                heading_id: h,
                                title: title.clone(),
                                body: body.clone(),
                            });
                        }
                        Err(e) => {
                            failed_items.insert(item_id.clone());
                            held.push(invalid(Some(item_id), None, format!("写不进 AFFiNE：{e}")));
                        }
                    }
                }
            }
        }
        if let Some(update) = delta(&local, &before) {
            affine.push(&run.ws.id, &d.doc_id, &update, false).await?;
            // 写完读一遍，和要写的对一下。
            let back: HashMap<String, AffineItem> = affine_items(&local)?
                .into_iter()
                .map(|a| (a.heading_id.clone(), a))
                .collect();
            let bad: Vec<&str> = written
                .iter()
                .filter(|(h, title, body)| {
                    back.get(h).is_none_or(|a| {
                        &a.title != title || !same_content(&a.blocks, &to_blocks(body))
                    })
                })
                .map(|(_, title, _)| title.as_str())
                .collect();
            if !bad.is_empty() {
                paused = Some(format!(
                    "写进 AFFiNE 以后读回来对不上：{}。看过以后：tcms affine sync --resume {path}",
                    bad.join("、")
                ));
            }
        }
    }

    // 写 git。
    let mut current = text.clone();
    let mut changed_items: Vec<String> = Vec::new();
    let defaults = parsed
        .as_ref()
        .map(|p| p.defaults.clone())
        .unwrap_or_default();
    let by_id: HashMap<&str, &ParsedItem> = parsed
        .as_ref()
        .map(|p| {
            p.items
                .iter()
                .filter_map(|it| Some((it.id.as_ref()?.as_str(), it)))
                .collect()
        })
        .unwrap_or_default();
    for op in &plan.to_git {
        let (item, heading, edit): (&str, Option<&str>, Result<Edit, String>) = match op {
            ToGit::Replace { id, title, body } => {
                let edit = by_id
                    .get(id.as_str())
                    .and_then(|p| ItemText::from_parsed(p))
                    .map(|mut t| {
                        t.title = normalize_title(title);
                        t.body = normalize_body(body);
                        Edit::Replace(t)
                    })
                    .ok_or_else(|| format!("文件里没有 {id}"));
                (id, None, edit)
            }
            ToGit::Remove { id } => (id, None, id.parse().map(Edit::Remove)),
            ToGit::Insert {
                id,
                after,
                heading_id,
                title,
                body,
            } => {
                let edit = new_item_text(id, title, body, &defaults).and_then(|item| match after {
                    Some(a) => Ok(Edit::InsertAfter {
                        after: a.parse()?,
                        item,
                    }),
                    None => Ok(Edit::Append(item)),
                });
                (id, Some(heading_id.as_str()), edit)
            }
        };
        let result = edit.and_then(|e| {
            apply(path, current.as_deref(), &e).map_err(|e: EditError| e.to_string())
        });
        match result {
            Ok(new) => {
                current = new;
                changed_items.push(item.to_owned());
                match op {
                    ToGit::Replace { .. } => report.to_git.updated += 1,
                    ToGit::Remove { .. } => report.to_git.removed += 1,
                    ToGit::Insert { .. } => report.to_git.added += 1,
                }
            }
            Err(e) => {
                failed_items.insert(item.to_owned());
                if let Some(h) = heading {
                    failed_headings.insert(h.to_owned());
                    held.push(invalid(None, Some(h), format!("写不进 git：{e}")));
                } else {
                    let h = doc_bases
                        .iter()
                        .find(|b| b.item_id == item)
                        .map(|b| b.heading_id.as_str());
                    held.push(invalid(Some(item), h, format!("写不进 git：{e}")));
                }
            }
        }
    }
    let mut git_blob = blob;
    if current != text {
        let author = run.editor(affine, &d.doc_id).await;
        let message = commit_message(path, &report.to_git, &changed_items, &author.name);
        let commit = run.batch.commit(
            cx.repo,
            &[FileEdit {
                path: path.to_owned(),
                content: current.map(String::into_bytes),
            }],
            &message,
            &author,
        )?;
        report.commit = Some(commit.to_string());
        git_blob = blob_at(cx.repo, Some(commit), path)?;
    }

    // 存底稿。写失败的那几条不改底稿，下次（处理完以后）再来。
    let mut set = Vec::new();
    let mut remove = Vec::new();
    for c in &plan.bases {
        match c {
            BaseChange::Set(b)
                if !failed_items.contains(&b.item_id)
                    && !failed_headings.contains(&b.heading_id) =>
            {
                set.push(AffineBase {
                    item_id: b.item_id.clone(),
                    path: path.to_owned(),
                    heading_id: b.heading_id.clone(),
                    title: normalize_title(&b.title),
                    body: normalize_body(&b.body),
                })
            }
            BaseChange::Remove { item_id } if !failed_items.contains(item_id) => {
                remove.push(item_id.clone())
            }
            _ => {}
        }
    }
    set.extend(extra_bases);
    report.held = held.len();
    if report.to_git.total() + report.to_affine.total() > 0 || !set.is_empty() || !remove.is_empty()
    {
        report.state = "synced";
    }
    if let Some(why) = &paused {
        report.state = "paused";
        report.paused = Some(why.clone());
    }
    if deleted_in_affine {
        // 人确认了：git 里归档完，这篇文档不再同步。
        cx.store.remove_affine_doc(path).await?;
        report.state = "unlinked";
        return Ok(report);
    }
    cx.store
        .save_affine_doc(&AffineSave {
            doc: AffineDoc {
                state: full_state(&local),
                git_blob,
                synced_commit: run.batch.head().map(|h| h.to_string()),
                paused,
                ..d.clone()
            },
            set,
            remove,
            conflicts: held.iter().map(|h| held_row(path, h)).collect(),
        })
        .await?;
    Ok(report)
}

/// AFFiNE 里范围文件夹下新建的文档：在 git 里那个范围目录下建一个文件（文件名是文档标题），每个二级标题一条。
async fn file_from_doc(
    cx: &Context<'_>,
    affine: &mut impl Affine,
    run: &mut Run<'_>,
    scope_row: &AffineScope,
    scope: &Scope,
    doc_id: &str,
    known_paths: &mut HashSet<String>,
) -> Result<DocReport, Error> {
    let mut report = DocReport::new("", Some(doc_id), "skipped");
    let found = affine.load(&run.ws.id, doc_id, None).await?;
    let Some(update) = found else {
        return Ok(report);
    };
    let local = Doc::new();
    apply_update(&local, &update)?;
    let page = affine_doc::read(&local)?;
    let title = match page.title.trim() {
        "" => run
            .ws
            .page(doc_id)
            .map(|p| p.title)
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| "未命名".to_owned()),
        t => t.to_owned(),
    };
    let sections = split(&page.blocks);
    let items: Vec<&affine_doc::Item> = sections
        .iter()
        .filter_map(|s| match s {
            Section::Item(i) if !i.title.is_empty() && i.heading_id.is_some() => Some(i),
            _ => None,
        })
        .collect();
    report.path = format!("{}/{}.md", scope.dir(), file_name(&title));
    if items.is_empty() {
        report
            .notes
            .push(format!("AFFiNE 里的「{title}」还没有二级标题，先不建文件"));
        return Ok(report);
    }
    let head_files: BTreeMap<String, ObjectId> = match run.batch.head() {
        Some(h) => cx.repo.files(h)?,
        None => BTreeMap::new(),
    };
    let stem = file_name(&title);
    let mut path = format!("{}/{stem}.md", scope.dir());
    let mut n = 2;
    while head_files.contains_key(&path) || known_paths.contains(&path) {
        path = format!("{}/{stem} {n}.md", scope.dir());
        n += 1;
    }
    report.path = path.clone();
    // 第一个二级标题前面的（一级标题、说明）照样写在文件开头。
    let preamble: Vec<Block> = sections
        .iter()
        .take_while(|s| matches!(s, Section::Loose(_)))
        .flat_map(|s| match s {
            Section::Loose(b) => b.clone(),
            Section::Item(_) => Vec::new(),
        })
        .collect();
    let mut text: Option<String> = if preamble.is_empty() {
        None
    } else {
        Some(format!("{}\n", to_markdown(&preamble)))
    };
    let mut bases = Vec::new();
    let mut held = Vec::new();
    for it in &items {
        let heading = it.heading_id.clone().unwrap_or_default();
        let id = ItemId::generate().to_string();
        let body = it.body_markdown();
        let result =
            new_item_text(&id, &it.title, &body, &FileDefaults::default()).and_then(|item| {
                apply(&path, text.as_deref(), &Edit::Append(item)).map_err(|e| e.to_string())
            });
        match result {
            Ok(new) => {
                text = new;
                bases.push(AffineBase {
                    item_id: id,
                    path: path.clone(),
                    heading_id: heading,
                    title: normalize_title(&it.title),
                    body: normalize_body(&body),
                });
            }
            Err(e) => held.push(invalid(None, Some(&heading), format!("写不进 git：{e}"))),
        }
    }
    if bases.is_empty() {
        report
            .notes
            .push(format!("「{title}」里的条目都写不进 git"));
        return Ok(report);
    }
    let author = run.editor(affine, doc_id).await;
    let ids: Vec<String> = bases.iter().map(|b| b.item_id.clone()).collect();
    report.to_git.added = bases.len();
    let message = commit_message(&path, &report.to_git, &ids, &author.name);
    let commit = run.batch.commit(
        cx.repo,
        &[FileEdit {
            path: path.clone(),
            content: text.map(String::into_bytes),
        }],
        &message,
        &author,
    )?;
    known_paths.insert(path.clone());
    cx.store
        .save_affine_doc(&AffineSave {
            doc: AffineDoc {
                path: path.clone(),
                scope: scope_row.scope.clone(),
                workspace_id: run.ws.id.clone(),
                doc_id: doc_id.to_owned(),
                state: full_state(&local),
                git_blob: blob_at(cx.repo, Some(commit), &path)?,
                synced_commit: Some(commit.to_string()),
                paused: None,
                synced_at: String::new(),
            },
            set: bases,
            remove: Vec::new(),
            conflicts: held.iter().map(|h| held_row(&path, h)).collect(),
        })
        .await?;
    report.state = "file_created";
    report.held = held.len();
    report.commit = Some(commit.to_string());
    Ok(report)
}

/// 文档标题做文件名：去掉路径分隔符和控制字符，太长的截短。
fn file_name(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| match c {
            '/' | '\\' => '-',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let cleaned = cleaned.trim_matches('.').trim();
    let short: String = cleaned.chars().take(80).collect();
    if short.is_empty() {
        "未命名".to_owned()
    } else {
        short
    }
}

// ---------------------------------------------------------------- 看状态、处理冲突

/// 接上的文件。
#[derive(Debug, Clone, Serialize)]
pub struct DocStatus {
    pub path: String,
    pub doc_id: String,
    pub items: usize,
    pub paused: Option<String>,
    pub synced_at: String,
}

/// `tcms affine status`。
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub scopes: Vec<AffineScope>,
    pub docs: Vec<DocStatus>,
    pub conflicts: Vec<AffineConflict>,
}

pub async fn status(store: &Store) -> Result<Status, Error> {
    let bases = store.affine_bases().await?;
    let mut count: HashMap<&str, usize> = HashMap::new();
    for b in &bases {
        *count.entry(b.path.as_str()).or_default() += 1;
    }
    let docs = store
        .affine_docs()
        .await?
        .into_iter()
        .map(|d| DocStatus {
            items: count.get(d.path.as_str()).copied().unwrap_or(0),
            path: d.path,
            doc_id: d.doc_id,
            paused: d.paused,
            synced_at: d.synced_at,
        })
        .collect();
    Ok(Status {
        scopes: store.affine_scopes().await?,
        docs,
        conflicts: store.affine_conflicts().await?,
    })
}

/// 处理冲突时留哪一边。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    Git,
    Affine,
}

/// 处理一条等人处理的：改底稿，让下次同步把留下的那边写到另一边。返回它所在的文件。
///
/// 底稿改成“不留的那边”的样子，下次同步时留下的那边就算“改过”、另一边“没改”，照常写过去：
/// 留 git 就把底稿改成 AFFiNE 的样子，留 AFFiNE 就改成 git 的样子。一边删了的，按要不要留下删掉对应关系。
pub async fn resolve(store: &Store, id: i64, keep: Keep) -> Result<String, Error> {
    let Some(c) = store
        .affine_conflicts()
        .await?
        .into_iter()
        .find(|c| c.id == id)
    else {
        return Err(Error::Invalid(format!(
            "没有第 {id} 条等人处理的（tcms affine status 看）"
        )));
    };
    let reason = Reason::parse(&c.reason).unwrap_or(Reason::Invalid);
    let base = |title: &Option<String>, body: &Option<String>| -> Option<AffineBase> {
        Some(AffineBase {
            item_id: c.item_id.clone()?,
            path: c.path.clone(),
            heading_id: c.heading_id.clone()?,
            title: title.clone()?,
            body: body.clone()?,
        })
    };
    let unmap = c.item_id.as_deref().map(|i| (c.path.as_str(), i));
    let (set, remove) = match (reason, keep) {
        (Reason::Both | Reason::Readonly | Reason::Invalid, Keep::Git) => {
            (base(&c.affine_title, &c.affine_body), None)
        }
        (Reason::Both | Reason::Readonly, Keep::Affine) => (base(&c.git_title, &c.git_body), None),
        // 写不过去的：删掉这条记录，下次再试。
        (Reason::Invalid, Keep::Affine) => (None, None),
        // AFFiNE 里删了、git 里改了：留 git 就去掉对应关系（下次当成 git 里新加的写进 AFFiNE）；
        // 留 AFFiNE（删掉）就把底稿改成 git 的样子，下次在 git 里归档。
        (Reason::AffineDeleted, Keep::Git) => (None, unmap),
        (Reason::AffineDeleted, Keep::Affine) => (base(&c.git_title, &c.git_body), None),
        // git 里删了、AFFiNE 里改了：留 git（删掉）就把底稿改成 AFFiNE 的样子，下次在 AFFiNE 里删掉；
        // 留 AFFiNE 就去掉对应关系，下次当成 AFFiNE 里新加的写进 git（新的 id）。
        (Reason::GitDeleted, Keep::Git) => (base(&c.affine_title, &c.affine_body), None),
        (Reason::GitDeleted, Keep::Affine) => (None, unmap),
    };
    store
        .resolve_affine_conflict(id, set.as_ref(), remove)
        .await?;
    Ok(c.path)
}
