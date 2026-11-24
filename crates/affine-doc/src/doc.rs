//! 用 yrs 读写一篇 AFFiNE 文档（`docs/affine-sync.md` 第 2.1 节）。
//!
//! 一篇文档一个 Yjs 文档，`blocks` 是块 id 到块的表，每个块是一个 Y.Map：`sys:id`、`sys:flavour`、
//! `sys:version`、`sys:children`（子块 id 的数组）和 `prop:*`。树是 `affine:page` → `affine:surface`
//! 和一个或几个 `affine:note` → 段落、列表、代码……。页面模式里看得到的是 `displayMode` 不是
//! `edgeless` 的那几个 note，按顺序接起来就是正文。
//!
//! 写的时候只写认得的几种块，属性只写用到的几个：AFFiNE 打开文档时会给缺的属性补上默认值
//! （BlockSuite `sync-controller.ts` 里“Set default props if not exists”）。

use similar::{Algorithm, DiffTag, capture_diff_slices};
use yrs::types::Attrs;
use yrs::types::text::YChange;
use yrs::{
    Any, Array, ArrayPrelim, ArrayRef, Doc, Map, MapPrelim, MapRef, Out, ReadTxn, Text, TextPrelim,
    TextRef, Transact, TransactionMut,
};

use crate::markdown::{is_empty_paragraph, key};
use crate::model::{
    Block, Content, ListKind, Marks, ParagraphKind, Span, normalize_spans, plain_text,
};

/// 文档里放块的那张表。
const BLOCKS: &str = "blocks";

const PAGE: &str = "affine:page";
const SURFACE: &str = "affine:surface";
const NOTE: &str = "affine:note";
const PARAGRAPH: &str = "affine:paragraph";
const LIST: &str = "affine:list";
const CODE: &str = "affine:code";
const DIVIDER: &str = "affine:divider";

/// 各种块在 AFFiNE 0.27.5 里的版本号（BlockSuite 的 `metadata.version`）。
fn version_of(flavour: &str) -> i64 {
    match flavour {
        PAGE => 2,
        SURFACE => 5,
        _ => 1,
    }
}

/// 嵌套最多读几层，防止坏数据里有环。
const MAX_DEPTH: usize = 64;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("文档里没有 affine:page 块")]
    NoPage,
    #[error("文档里已经有块了，不能当新文档写")]
    NotEmpty,
    #[error("页面正文里找不到块 {0}")]
    NotFound(String),
    #[error("块 {0} 不是二级标题")]
    NotHeading(String),
    #[error("一条的正文里不能有一级、二级标题")]
    HeadingInBody,
}

/// 新块的 id 从哪里来。
pub trait Ids {
    fn next_id(&mut self) -> String;
}

/// 随机的 id，和 AFFiNE 自己的差不多：10 个字母、数字、`_`、`-`。
#[derive(Debug, Default)]
pub struct RandomIds;

impl Ids for RandomIds {
    fn next_id(&mut self) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
        let mut buf = [0u8; 10];
        // 拿不到系统随机数时退回用时间和计数拼，只影响 id 好不好看，不影响对错。
        if getrandom::fill(&mut buf).is_err() {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            for (i, b) in buf.iter_mut().enumerate() {
                *b = (t >> (i * 6)) as u8;
            }
        }
        buf.iter()
            .map(|b| ALPHABET[usize::from(b & 63)] as char)
            .collect()
    }
}

/// 读出来的一篇文档。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub title: String,
    /// 页面模式里看得到的正文，最上一层的块按顺序。
    pub blocks: Vec<Block>,
}

// ---------------------------------------------------------------- 读

/// 读一篇文档。
pub fn read(doc: &Doc) -> Result<Page, Error> {
    let map = doc.get_or_insert_map(BLOCKS);
    let txn = doc.transact();
    let page = find_page(&map, &txn).ok_or(Error::NoPage)?;
    let title = match page.get(&txn, "prop:title") {
        Some(v) => plain_text(&read_text(&txn, &v).0).replace('\n', " "),
        None => String::new(),
    };
    let mut blocks = Vec::new();
    for (_, children) in visible_notes(&map, &txn, &page) {
        for id in child_ids(&txn, &children) {
            if let Some(b) = read_block(&map, &txn, &id, 0) {
                blocks.push(b);
            }
        }
    }
    Ok(Page { title, blocks })
}

fn find_page<T: ReadTxn>(map: &MapRef, txn: &T) -> Option<MapRef> {
    map.iter(txn).find_map(|(_, v)| match v {
        Out::YMap(m) if str_prop(&m, txn, "sys:flavour").as_deref() == Some(PAGE) => Some(m),
        _ => None,
    })
}

fn block_map<T: ReadTxn>(map: &MapRef, txn: &T, id: &str) -> Option<MapRef> {
    match map.get(txn, id) {
        Some(Out::YMap(m)) => Some(m),
        _ => None,
    }
}

fn str_prop<T: ReadTxn>(m: &MapRef, txn: &T, key: &str) -> Option<String> {
    match m.get(txn, key) {
        Some(Out::Any(Any::String(s))) => Some(s.to_string()),
        _ => None,
    }
}

fn bool_prop<T: ReadTxn>(m: &MapRef, txn: &T, key: &str) -> bool {
    matches!(m.get(txn, key), Some(Out::Any(Any::Bool(true))))
}

fn children_array<T: ReadTxn>(m: &MapRef, txn: &T) -> Option<ArrayRef> {
    match m.get(txn, "sys:children") {
        Some(Out::YArray(a)) => Some(a),
        _ => None,
    }
}

fn child_ids<T: ReadTxn>(txn: &T, a: &ArrayRef) -> Vec<String> {
    a.iter(txn)
        .filter_map(|v| match v {
            Out::Any(Any::String(s)) => Some(s.to_string()),
            _ => None,
        })
        .collect()
}

/// 页面模式里看得到的 note：（note 的 id，它的子块数组）。
fn visible_notes<T: ReadTxn>(map: &MapRef, txn: &T, page: &MapRef) -> Vec<(String, ArrayRef)> {
    let Some(children) = children_array(page, txn) else {
        return Vec::new();
    };
    child_ids(txn, &children)
        .into_iter()
        .filter_map(|id| {
            let note = block_map(map, txn, &id)?;
            if str_prop(&note, txn, "sys:flavour").as_deref() != Some(NOTE) {
                return None;
            }
            if str_prop(&note, txn, "prop:displayMode").as_deref() == Some("edgeless") {
                return None;
            }
            Some((id, children_array(&note, txn)?))
        })
        .collect()
}

/// 读带格式的文字。第二个值是有没有丢东西（不认得的格式、嵌进去的对象）。
fn read_text<T: ReadTxn>(txn: &T, v: &Out) -> (Vec<Span>, bool) {
    match v {
        Out::YText(t) => {
            let mut lossy = false;
            let mut spans = Vec::new();
            for d in t.diff(txn, YChange::identity) {
                let Out::Any(Any::String(s)) = &d.insert else {
                    lossy = true;
                    continue;
                };
                let (marks, l) = marks_of(d.attributes.as_deref());
                lossy |= l;
                spans.push(Span {
                    text: s.to_string(),
                    marks,
                });
            }
            (normalize_spans(spans), lossy)
        }
        Out::Any(Any::String(s)) => (normalize_spans(vec![Span::plain(s.to_string())]), false),
        _ => (Vec::new(), true),
    }
}

fn marks_of(attrs: Option<&Attrs>) -> (Marks, bool) {
    let mut m = Marks::default();
    let mut lossy = false;
    for (k, v) in attrs.into_iter().flatten() {
        match (k.as_ref(), v) {
            (_, Any::Null | Any::Undefined | Any::Bool(false)) => {}
            ("bold", Any::Bool(true)) => m.bold = true,
            ("italic", Any::Bool(true)) => m.italic = true,
            ("strike", Any::Bool(true)) => m.strike = true,
            ("code", Any::Bool(true)) => m.code = true,
            ("link", Any::String(url)) => m.link = Some(url.to_string()),
            _ => lossy = true,
        }
    }
    (m, lossy)
}

fn read_block<T: ReadTxn>(map: &MapRef, txn: &T, id: &str, depth: usize) -> Option<Block> {
    let m = block_map(map, txn, id)?;
    let flavour = str_prop(&m, txn, "sys:flavour").unwrap_or_default();
    let children = || -> (Vec<Block>, bool) {
        if depth >= MAX_DEPTH {
            return (Vec::new(), true);
        }
        let ids = children_array(&m, txn)
            .map(|a| child_ids(txn, &a))
            .unwrap_or_default();
        let blocks: Vec<Block> = ids
            .iter()
            .filter_map(|c| read_block(map, txn, c, depth + 1))
            .collect();
        let lost = blocks.len() != ids.len();
        (blocks, lost)
    };
    let text = || {
        m.get(txn, "prop:text")
            .map(|v| read_text(txn, &v))
            .unwrap_or_default()
    };
    let (content, lossy) = match flavour.as_str() {
        PARAGRAPH => {
            let ty = str_prop(&m, txn, "prop:type").unwrap_or_else(|| "text".to_owned());
            let (kind, odd) = match ParagraphKind::from_affine(&ty) {
                Some(k) => (k, false),
                None => (ParagraphKind::Text, true),
            };
            let (text, l1) = text();
            let (children, l2) = children();
            // 段落的子块 Markdown 写不出来。
            let nested = !children.is_empty();
            (
                Content::Paragraph {
                    kind,
                    text,
                    children,
                },
                odd || l1 || l2 || nested,
            )
        }
        LIST => {
            let ty = str_prop(&m, txn, "prop:type").unwrap_or_else(|| "bulleted".to_owned());
            let (kind, odd) = match ty.as_str() {
                "numbered" => (ListKind::Numbered, false),
                "todo" => (
                    ListKind::Todo {
                        checked: bool_prop(&m, txn, "prop:checked"),
                    },
                    false,
                ),
                "bulleted" => (ListKind::Bulleted, false),
                // 折叠列表（toggle）等：当成无序列表。
                _ => (ListKind::Bulleted, true),
            };
            let (text, l1) = text();
            let (children, l2) = children();
            (
                Content::List {
                    kind,
                    text,
                    children,
                },
                odd || l1 || l2,
            )
        }
        CODE => {
            let language = str_prop(&m, txn, "prop:language").filter(|s| !s.is_empty());
            let (spans, l) = text();
            (
                Content::Code {
                    language,
                    text: plain_text(&spans),
                },
                l,
            )
        }
        DIVIDER => (Content::Divider, false),
        _ => (Content::Other { flavour }, false),
    };
    Some(Block {
        id: Some(id.to_owned()),
        content,
        lossy,
    })
}

// ---------------------------------------------------------------- 写

/// 把一篇空文档写成一篇新的：页面块、白板层、一个 note，正文是 `blocks`。
/// 返回没写进去的占位块的 id（新文档里没有它们）。
pub fn create(
    doc: &Doc,
    title: &str,
    blocks: &[Block],
    ids: &mut dyn Ids,
) -> Result<Vec<String>, Error> {
    let map = doc.get_or_insert_map(BLOCKS);
    let mut txn = doc.transact_mut();
    if map.len(&txn) > 0 {
        return Err(Error::NotEmpty);
    }
    let page_id = ids.next_id();
    let surface_id = ids.next_id();
    let note_id = ids.next_id();
    let page = new_block(&mut txn, &map, &page_id, PAGE);
    let t: TextRef = page.insert(&mut txn, "prop:title", TextPrelim::new(""));
    if !title.is_empty() {
        t.insert(&mut txn, 0, title);
    }
    push_child(&mut txn, &page, &surface_id);
    push_child(&mut txn, &page, &note_id);
    new_block(&mut txn, &map, &surface_id, SURFACE);
    let note = new_block(&mut txn, &map, &note_id, NOTE);
    note.insert(&mut txn, "prop:displayMode", "both");
    let mut skipped = Vec::new();
    for b in blocks {
        if let Some(id) = write_block(&mut txn, &map, b, ids, &mut skipped) {
            push_child(&mut txn, &note, &id);
        }
    }
    Ok(skipped)
}

/// 改文档标题。
pub fn set_title(doc: &Doc, title: &str) -> Result<(), Error> {
    let map = doc.get_or_insert_map(BLOCKS);
    let mut txn = doc.transact_mut();
    let page = find_page(&map, &txn).ok_or(Error::NoPage)?;
    match page.get(&txn, "prop:title") {
        Some(Out::YText(t)) => replace_text(&mut txn, &t, &[Span::plain(title)]),
        _ => {
            let t: TextRef = page.insert(&mut txn, "prop:title", TextPrelim::new(""));
            t.insert(&mut txn, 0, title);
        }
    }
    Ok(())
}

fn new_block(txn: &mut TransactionMut, map: &MapRef, id: &str, flavour: &str) -> MapRef {
    let m: MapRef = map.insert(txn, id, MapPrelim::default());
    m.insert(txn, "sys:id", id);
    m.insert(txn, "sys:flavour", flavour);
    m.insert(txn, "sys:version", Any::from(version_of(flavour)));
    m.insert(txn, "sys:children", ArrayPrelim::default());
    m
}

fn push_child(txn: &mut TransactionMut, parent: &MapRef, id: &str) {
    if let Some(a) = children_array(parent, txn) {
        a.push_back(txn, id);
    }
}

fn attrs_of(m: &Marks) -> Attrs {
    let mut a = Attrs::new();
    if m.bold {
        a.insert("bold".into(), true.into());
    }
    if m.italic {
        a.insert("italic".into(), true.into());
    }
    if m.strike {
        a.insert("strike".into(), true.into());
    }
    if m.code {
        a.insert("code".into(), true.into());
    }
    if let Some(url) = &m.link {
        a.insert("link".into(), url.as_str().into());
    }
    a
}

fn write_spans(txn: &mut TransactionMut, t: &TextRef, spans: &[Span]) {
    for s in spans {
        if s.text.is_empty() {
            continue;
        }
        // 总是带上属性写：不带属性的插入会沿用前一个字的格式。
        let at = t.len(txn);
        t.insert_with_attributes(txn, at, &s.text, attrs_of(&s.marks));
    }
}

fn replace_text(txn: &mut TransactionMut, t: &TextRef, spans: &[Span]) {
    let len = t.len(txn);
    if len > 0 {
        t.remove_range(txn, 0, len);
    }
    write_spans(txn, t, spans);
}

/// 写一个新块（连同子块），返回它的 id。占位块不新建：返回它原来的 id，
/// 原来那个块不在文档里时记进 `skipped`、返回 `None`。
fn write_block(
    txn: &mut TransactionMut,
    map: &MapRef,
    b: &Block,
    ids: &mut dyn Ids,
    skipped: &mut Vec<String>,
) -> Option<String> {
    if let Content::Other { .. } = &b.content {
        let id = b.id.clone()?;
        if block_map(map, txn, &id).is_some() {
            return Some(id);
        }
        skipped.push(id);
        return None;
    }
    let id = ids.next_id();
    let (flavour, children) = match &b.content {
        Content::Paragraph { children, .. } => (PARAGRAPH, children.as_slice()),
        Content::List { children, .. } => (LIST, children.as_slice()),
        Content::Code { .. } => (CODE, &[][..]),
        Content::Divider | Content::Other { .. } => (DIVIDER, &[][..]),
    };
    let m = new_block(txn, map, &id, flavour);
    match &b.content {
        Content::Paragraph { kind, text, .. } => {
            m.insert(txn, "prop:type", kind.as_affine());
            let t: TextRef = m.insert(txn, "prop:text", TextPrelim::new(""));
            write_spans(txn, &t, text);
        }
        Content::List { kind, text, .. } => {
            let (ty, checked) = match kind {
                ListKind::Bulleted => ("bulleted", false),
                ListKind::Numbered => ("numbered", false),
                ListKind::Todo { checked } => ("todo", *checked),
            };
            m.insert(txn, "prop:type", ty);
            m.insert(txn, "prop:checked", checked);
            let t: TextRef = m.insert(txn, "prop:text", TextPrelim::new(""));
            write_spans(txn, &t, text);
        }
        Content::Code { language, text } => {
            let language = match language {
                Some(l) => Any::from(l.as_str()),
                None => Any::Null,
            };
            m.insert(txn, "prop:language", language);
            let t: TextRef = m.insert(txn, "prop:text", TextPrelim::new(""));
            if !text.is_empty() {
                t.insert(txn, 0, text);
            }
        }
        Content::Divider | Content::Other { .. } => {}
    }
    for c in children {
        if let Some(cid) = write_block(txn, map, c, ids, skipped) {
            push_child(txn, &m, &cid);
        }
    }
    Some(id)
}

/// 删掉一个块和它下面所有的块；`keep` 里的（还被占位块用着的）不删。
fn delete_block(txn: &mut TransactionMut, map: &MapRef, id: &str, keep: &[String], depth: usize) {
    if keep.iter().any(|k| k == id) {
        return;
    }
    if let Some(m) = block_map(map, txn, id)
        && depth < MAX_DEPTH
        && let Some(a) = children_array(&m, txn)
    {
        for c in child_ids(txn, &a) {
            delete_block(txn, map, &c, keep, depth + 1);
        }
    }
    map.remove(txn, id);
}

/// 新内容里所有占位块的 id（连子块里的）。
fn placeholder_ids(blocks: &[Block], out: &mut Vec<String>) {
    for b in blocks {
        match &b.content {
            Content::Other { .. } => out.extend(b.id.clone()),
            Content::Paragraph { children, .. } | Content::List { children, .. } => {
                placeholder_ids(children, out)
            }
            _ => {}
        }
    }
}

/// 去掉重复的 id（同一个占位块写了两次时只留第一个）。
fn dedup(ids: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    ids.into_iter().filter(|i| seen.insert(i.clone())).collect()
}

// ---------------------------------------------------------------- 按条目改

/// 一个块在页面正文最上一层的位置。
struct Located {
    children: ArrayRef,
    index: u32,
    /// 同一个 note 里所有块的 id。
    ids: Vec<String>,
}

fn locate<T: ReadTxn>(map: &MapRef, txn: &T, id: &str) -> Result<Located, Error> {
    let page = find_page(map, txn).ok_or(Error::NoPage)?;
    for (_, children) in visible_notes(map, txn, &page) {
        let ids = child_ids(txn, &children);
        if let Some(i) = ids.iter().position(|x| x == id) {
            return Ok(Located {
                children,
                index: i as u32,
                ids,
            });
        }
    }
    Err(Error::NotFound(id.to_owned()))
}

fn top_heading_level<T: ReadTxn>(map: &MapRef, txn: &T, id: &str) -> Option<u8> {
    let m = block_map(map, txn, id)?;
    if str_prop(&m, txn, "sys:flavour").as_deref() != Some(PARAGRAPH) {
        return None;
    }
    match ParagraphKind::from_affine(&str_prop(&m, txn, "prop:type")?)? {
        ParagraphKind::Heading(l) => Some(l),
        _ => None,
    }
}

/// 一条的正文：标题后面、下一个一级或二级标题前面的块。
fn body_after<T: ReadTxn>(map: &MapRef, txn: &T, ids: &[String], heading: usize) -> Vec<String> {
    ids[heading + 1..]
        .iter()
        .take_while(|id| !top_heading_level(map, txn, id).is_some_and(|l| l <= 2))
        .cloned()
        .collect()
}

fn check_body(body: &[Block]) -> Result<(), Error> {
    if body
        .iter()
        .any(|b| b.heading_level().is_some_and(|l| l <= 2))
    {
        return Err(Error::HeadingInBody);
    }
    Ok(())
}

/// 占位块指向的块在别处（页面最上一层的别的地方）时，先从那里拿下来，好放到新位置。
fn detach_moved(txn: &mut TransactionMut, map: &MapRef, body: &[Block], keep_in: &[String]) {
    for b in body {
        let (Content::Other { .. }, Some(id)) = (&b.content, &b.id) else {
            continue;
        };
        if keep_in.contains(id) {
            continue;
        }
        if let Ok(at) = locate(map, txn, id) {
            at.children.remove(txn, at.index);
        }
    }
}

/// 改一条：标题改成 `title`，正文换成 `body`。只动变了的块：转成 Markdown 一样的块原样留着
/// （AFFiNE 里 Markdown 写不出的格式也就留住了），空段落留在原地。标题块的 id 不变。
/// 返回没找到的占位块的 id。
pub fn update_item(
    doc: &Doc,
    heading_id: &str,
    title: &str,
    body: &[Block],
    ids: &mut dyn Ids,
) -> Result<Vec<String>, Error> {
    check_body(body)?;
    let map = doc.get_or_insert_map(BLOCKS);
    let mut txn = doc.transact_mut();
    let at = locate(&map, &txn, heading_id)?;
    if top_heading_level(&map, &txn, heading_id) != Some(2) {
        return Err(Error::NotHeading(heading_id.to_owned()));
    }
    let run = body_after(&map, &txn, &at.ids, at.index as usize);
    detach_moved(&mut txn, &map, body, &run);
    let at = locate(&map, &txn, heading_id)?;
    let run = body_after(&map, &txn, &at.ids, at.index as usize);

    let heading =
        block_map(&map, &txn, heading_id).ok_or_else(|| Error::NotFound(heading_id.to_owned()))?;
    if let Some(Out::YText(t)) = heading.get(&txn, "prop:text") {
        let current = plain_text(&read_text(&txn, &Out::YText(t.clone())).0);
        if current != title {
            replace_text(&mut txn, &t, &[Span::plain(title)]);
        }
    }
    let mut skipped = Vec::new();
    splice(
        &mut txn,
        &map,
        &at.children,
        at.index + 1,
        &run,
        body,
        ids,
        &mut skipped,
    );
    Ok(skipped)
}

/// 加一条：放在 `after` 那一条的正文后面；`None` 时放在正文最后。返回新标题块的 id 和没找到的占位块。
pub fn insert_item(
    doc: &Doc,
    after: Option<&str>,
    title: &str,
    body: &[Block],
    ids: &mut dyn Ids,
) -> Result<(String, Vec<String>), Error> {
    check_body(body)?;
    let map = doc.get_or_insert_map(BLOCKS);
    let mut txn = doc.transact_mut();
    detach_moved(&mut txn, &map, body, &[]);
    let (children, index) = match after {
        Some(h) => {
            let at = locate(&map, &txn, h)?;
            let run = body_after(&map, &txn, &at.ids, at.index as usize);
            (at.children, at.index + 1 + run.len() as u32)
        }
        None => {
            let page = find_page(&map, &txn).ok_or(Error::NoPage)?;
            match visible_notes(&map, &txn, &page).pop() {
                Some((_, children)) => {
                    let len = children.len(&txn);
                    (children, len)
                }
                None => {
                    let note_id = ids.next_id();
                    let note = new_block(&mut txn, &map, &note_id, NOTE);
                    note.insert(&mut txn, "prop:displayMode", "both");
                    push_child(&mut txn, &page, &note_id);
                    let children = children_array(&note, &txn).ok_or(Error::NoPage)?;
                    (children, 0)
                }
            }
        }
    };
    let mut skipped = Vec::new();
    let heading = Block::paragraph(ParagraphKind::Heading(2), vec![Span::plain(title)]);
    let mut new_ids = Vec::new();
    for b in std::iter::once(&heading).chain(body) {
        if let Some(id) = write_block(&mut txn, &map, b, ids, &mut skipped) {
            new_ids.push(id);
        }
    }
    let heading_id = new_ids[0].clone();
    insert_ids(&mut txn, &children, index, &dedup(new_ids));
    Ok((heading_id, skipped))
}

/// 删掉一条：标题块和正文的块。
pub fn remove_item(doc: &Doc, heading_id: &str) -> Result<(), Error> {
    let map = doc.get_or_insert_map(BLOCKS);
    let mut txn = doc.transact_mut();
    let at = locate(&map, &txn, heading_id)?;
    if top_heading_level(&map, &txn, heading_id) != Some(2) {
        return Err(Error::NotHeading(heading_id.to_owned()));
    }
    let run = body_after(&map, &txn, &at.ids, at.index as usize);
    at.children
        .remove_range(&mut txn, at.index, 1 + run.len() as u32);
    delete_block(&mut txn, &map, heading_id, &[], 0);
    for id in &run {
        delete_block(&mut txn, &map, id, &[], 0);
    }
    Ok(())
}

fn insert_ids(txn: &mut TransactionMut, children: &ArrayRef, index: u32, ids: &[String]) {
    for (k, id) in ids.iter().enumerate() {
        children.insert(txn, index + k as u32, id.as_str());
    }
}

/// 把 `children` 里从 `start` 起的 `old`（一串块的 id）换成 `new`。
#[allow(clippy::too_many_arguments)]
fn splice(
    txn: &mut TransactionMut,
    map: &MapRef,
    children: &ArrayRef,
    start: u32,
    old: &[String],
    new: &[Block],
    ids: &mut dyn Ids,
    skipped: &mut Vec<String>,
) {
    let old_blocks: Vec<Option<Block>> = old.iter().map(|id| read_block(map, txn, id, 0)).collect();
    // 空段落和读不出来的块不参加比较，留在原地：跟着前面那个块走。
    let significant: Vec<usize> = (0..old.len())
        .filter(|&i| {
            old_blocks[i]
                .as_ref()
                .is_some_and(|b| !is_empty_paragraph(b))
        })
        .collect();
    let old_keys: Vec<String> = significant
        .iter()
        .map(|&i| old_blocks[i].as_ref().map(key).unwrap_or_default())
        .collect();
    let new_keys: Vec<String> = new.iter().map(key).collect();
    // 占位块指向的块不删。
    let mut keep = Vec::new();
    placeholder_ids(new, &mut keep);

    // 某个参加比较的块后面跟着的、不参加比较的块。
    let trailing = |k: usize| -> &[String] {
        let from = significant[k] + 1;
        let to = significant.get(k + 1).copied().unwrap_or(old.len());
        &old[from..to]
    };
    let mut target: Vec<String> = Vec::with_capacity(old.len() + new.len());
    target.extend_from_slice(&old[..significant.first().copied().unwrap_or(old.len())]);
    let mut deleted: Vec<String> = Vec::new();
    for op in capture_diff_slices(Algorithm::Myers, &old_keys, &new_keys) {
        let (tag, o, n) = op.as_tag_tuple();
        match tag {
            DiffTag::Equal => {
                for k in o {
                    target.push(old[significant[k]].clone());
                    target.extend_from_slice(trailing(k));
                }
            }
            DiffTag::Delete | DiffTag::Insert | DiffTag::Replace => {
                for b in &new[n] {
                    if let Some(id) = write_block(txn, map, b, ids, skipped) {
                        target.push(id);
                    }
                }
                for k in o {
                    deleted.push(old[significant[k]].clone());
                    target.extend_from_slice(trailing(k));
                }
            }
        }
    }

    let target = dedup(target);
    // 数组上只做必要的增删。
    let mut cursor = start;
    for op in capture_diff_slices(Algorithm::Myers, old, &target) {
        let (tag, o, n) = op.as_tag_tuple();
        match tag {
            DiffTag::Equal => cursor += o.len() as u32,
            DiffTag::Delete => children.remove_range(txn, cursor, o.len() as u32),
            DiffTag::Insert => {
                insert_ids(txn, children, cursor, &target[n.clone()]);
                cursor += n.len() as u32;
            }
            DiffTag::Replace => {
                children.remove_range(txn, cursor, o.len() as u32);
                insert_ids(txn, children, cursor, &target[n.clone()]);
                cursor += n.len() as u32;
            }
        }
    }
    for id in deleted {
        delete_block(txn, map, &id, &keep, 0);
    }
}
