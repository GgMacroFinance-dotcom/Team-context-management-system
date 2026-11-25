//! 工作区一级的结构：根文档里的文档列表，和放在单独 Yjs 文档里的几张表（文件夹树、文档属性）。
//!
//! 依据是 AFFiNE 0.27.5 的 MIT 部分（只读了设计，没复制代码）：
//!
//! - 根文档（guid 就是工作区 id）里有一个 Y.Map `meta`：`name`、`avatar`、`pages`（每篇文档一项，
//!   BlockSuite 写进去时会把普通对象转成 Y.Map、数组转成 Y.Array）、`properties`（标签的选项）。
//!   见 `packages/frontend/core/src/modules/workspace/impls/meta.ts`、
//!   `blocksuite/framework/store/src/extension/workspace/workspace-meta.ts`。
//! - 工作区的“数据库”每张表一篇 Yjs 文档，服务端的文档 id 是 `db$<工作区 id>$<表名>`：根上每个键是一行（一个 Y.Map），
//!   键就是这一行的主键；删掉的行只留主键，另加 `$$DELETED: true`。见
//!   `packages/common/infra/src/orm/core/adapters/yjs/table.ts` 和
//!   `packages/frontend/core/src/modules/db/schema/schema.ts`。
//!
//! 0.27.5 的服务端还另有一份“工作区状态”（同步协议的 `space:load-workspace-state`），开了它以后
//! 文档列表以服务端为准、`meta.pages` 只补标题这类字段。连服务端的那一层（A2）核对以后再接。

use std::collections::BTreeMap;

use yrs::types::ToJson as _;
use yrs::{Any, Array, ArrayPrelim, ArrayRef, Doc, Map, MapPrelim, MapRef, Out, ReadTxn, Transact};

use crate::doc::Ids;
use crate::order::{self, OrderError};

// ---------------------------------------------------------------- 根文档：文档列表

const META: &str = "meta";

/// `meta.pages` 里的一项。
#[derive(Clone, Debug, PartialEq)]
pub struct DocMeta {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    /// 毫秒时间戳。
    pub create_date: Option<f64>,
    pub updated_date: Option<f64>,
    pub favorite: bool,
    pub trash: bool,
    /// 认不得的字段原样带着（比如 `trashDate`）。
    pub extra: BTreeMap<String, Any>,
}

impl DocMeta {
    pub fn new(id: impl Into<String>, title: impl Into<String>, create_date: f64) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            tags: Vec::new(),
            create_date: Some(create_date),
            updated_date: None,
            favorite: false,
            trash: false,
            extra: BTreeMap::new(),
        }
    }
}

/// 根文档的 `meta`。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorkspaceMeta {
    pub name: Option<String>,
    pub avatar: Option<String>,
    pub pages: Vec<DocMeta>,
}

fn any_str(v: &Any) -> Option<String> {
    match v {
        Any::String(s) => Some(s.to_string()),
        _ => None,
    }
}

fn any_num(v: &Any) -> Option<f64> {
    match v {
        Any::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn doc_meta_from(fields: BTreeMap<String, Any>) -> Option<DocMeta> {
    let mut m = DocMeta {
        id: String::new(),
        title: String::new(),
        tags: Vec::new(),
        create_date: None,
        updated_date: None,
        favorite: false,
        trash: false,
        extra: BTreeMap::new(),
    };
    for (k, v) in fields {
        match k.as_str() {
            "id" => m.id = any_str(&v)?,
            "title" => m.title = any_str(&v).unwrap_or_default(),
            "tags" => {
                if let Any::Array(a) = &v {
                    m.tags = a.iter().filter_map(any_str).collect();
                }
            }
            "createDate" => m.create_date = any_num(&v),
            "updatedDate" => m.updated_date = any_num(&v),
            "favorite" => m.favorite = matches!(v, Any::Bool(true)),
            "trash" => m.trash = matches!(v, Any::Bool(true)),
            _ => {
                m.extra.insert(k, v);
            }
        }
    }
    (!m.id.is_empty()).then_some(m)
}

/// 一个值连同里面的 Y 类型整个转成普通值。
fn plain<T: ReadTxn>(txn: &T, v: &Out) -> Any {
    v.to_json(txn)
}

fn as_fields(v: Any) -> Option<BTreeMap<String, Any>> {
    match v {
        Any::Map(m) => Some(m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        _ => None,
    }
}

/// 读根文档的 `meta`。`pages` 里格式不对的项跳过。
pub fn read_meta(root: &Doc) -> WorkspaceMeta {
    let meta = root.get_or_insert_map(META);
    let txn = root.transact();
    let mut out = WorkspaceMeta {
        name: meta
            .get(&txn, "name")
            .and_then(|v| any_str(&plain(&txn, &v))),
        avatar: meta
            .get(&txn, "avatar")
            .and_then(|v| any_str(&plain(&txn, &v))),
        pages: Vec::new(),
    };
    if let Some(Any::Array(pages)) = meta.get(&txn, "pages").map(|v| plain(&txn, &v)) {
        out.pages = pages
            .iter()
            .cloned()
            .filter_map(as_fields)
            .filter_map(doc_meta_from)
            .collect();
    }
    out
}

fn pages_array(meta: &MapRef, txn: &mut yrs::TransactionMut) -> ArrayRef {
    match meta.get(txn, "pages") {
        Some(Out::YArray(a)) => a,
        _ => meta.insert(txn, "pages", ArrayPrelim::default()),
    }
}

fn find_page_index<T: ReadTxn>(txn: &T, pages: &ArrayRef, id: &str) -> Option<u32> {
    pages
        .iter(txn)
        .position(|v| match v {
            Out::YMap(m) => matches!(m.get(txn, "id"), Some(Out::Any(Any::String(s))) if &*s == id),
            Out::Any(Any::Map(m)) => matches!(m.get("id"), Some(Any::String(s)) if &**s == id),
            _ => false,
        })
        .map(|i| i as u32)
}

fn write_doc_meta(txn: &mut yrs::TransactionMut, m: &MapRef, d: &DocMeta) {
    m.insert(txn, "id", d.id.as_str());
    m.insert(txn, "title", d.title.as_str());
    let tags: ArrayRef = m.insert(txn, "tags", ArrayPrelim::default());
    for t in &d.tags {
        tags.push_back(txn, t.as_str());
    }
    if let Some(c) = d.create_date {
        m.insert(txn, "createDate", c);
    }
    if let Some(u) = d.updated_date {
        m.insert(txn, "updatedDate", u);
    }
    if d.favorite {
        m.insert(txn, "favorite", true);
    } else {
        m.remove(txn, "favorite");
    }
    if d.trash {
        m.insert(txn, "trash", true);
    } else {
        m.remove(txn, "trash");
    }
    for (k, v) in &d.extra {
        m.insert(txn, k.as_str(), v.clone());
    }
}

/// 在文档列表里加一篇（放在最后）。已经有同一个 id 的就改成新的内容。
pub fn upsert_doc_meta(root: &Doc, d: &DocMeta) {
    let meta = root.get_or_insert_map(META);
    let mut txn = root.transact_mut();
    let pages = pages_array(&meta, &mut txn);
    match find_page_index(&txn, &pages, &d.id) {
        Some(i) => match pages.get(&txn, i) {
            Some(Out::YMap(m)) => write_doc_meta(&mut txn, &m, d),
            _ => {
                // 别人写成了普通对象：换成 Y.Map，和 BlockSuite 写的一样。
                pages.remove(&mut txn, i);
                let m = pages.insert(&mut txn, i, MapPrelim::default());
                write_doc_meta(&mut txn, &m, d);
            }
        },
        None => {
            let m = pages.push_back(&mut txn, MapPrelim::default());
            write_doc_meta(&mut txn, &m, d);
        }
    }
}

/// 从文档列表里去掉一篇（不碰那篇文档本身）。没有就什么也不做，返回 `false`。
pub fn remove_doc_meta(root: &Doc, id: &str) -> bool {
    let meta = root.get_or_insert_map(META);
    let mut txn = root.transact_mut();
    let Some(Out::YArray(pages)) = meta.get(&txn, "pages") else {
        return false;
    };
    match find_page_index(&txn, &pages, id) {
        Some(i) => {
            pages.remove(&mut txn, i);
            true
        }
        None => false,
    }
}

/// 改工作区的名字。
pub fn set_workspace_name(root: &Doc, name: &str) {
    let meta = root.get_or_insert_map(META);
    let mut txn = root.transact_mut();
    meta.insert(&mut txn, "name", name);
}

// ---------------------------------------------------------------- 表

/// 删掉的行带的标记。
const DELETED: &str = "$$DELETED";

/// 工作区 `workspace_id` 的表 `name` 存在哪篇 Yjs 文档里：服务端的文档 id 是
/// `db$<工作区 id>$<表名>`。只写 `db$<表名>` 时 AFFiNE 0.27.5 读报 `DOC_ACTION_DENIED`、写报
/// `ACCESS_DENIED`（A3a 对着 Docker 里的 0.27.5-beta.3 核对的）。
pub fn table_doc_id(workspace_id: &str, name: &str) -> String {
    format!("db${workspace_id}${name}")
}

/// 表里的一行。
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub key: String,
    pub fields: BTreeMap<String, Any>,
}

impl Row {
    pub fn str(&self, field: &str) -> Option<&str> {
        match self.fields.get(field) {
            Some(Any::String(s)) => Some(s),
            _ => None,
        }
    }
}

fn root_names(doc: &Doc) -> Vec<String> {
    let txn = doc.transact();
    txn.root_refs().map(|(name, _)| name.to_owned()).collect()
}

fn read_row(doc: &Doc, key: &str) -> Option<Row> {
    let m = doc.get_or_insert_map(key);
    let txn = doc.transact();
    let fields: BTreeMap<String, Any> = m
        .iter(&txn)
        .map(|(k, v)| (k.to_owned(), plain(&txn, &v)))
        .collect();
    if fields.is_empty() || matches!(fields.get(DELETED), Some(Any::Bool(true))) {
        return None;
    }
    Some(Row {
        key: key.to_owned(),
        fields,
    })
}

/// 表里所有没删掉的行，按主键排好。
pub fn rows(table: &Doc) -> Vec<Row> {
    let mut names = root_names(table);
    names.sort();
    names.iter().filter_map(|k| read_row(table, k)).collect()
}

/// 按主键取一行。没有或已删掉都是 `None`。
pub fn row(table: &Doc, key: &str) -> Option<Row> {
    if !root_names(table).iter().any(|n| n == key) {
        // 不先看一眼的话 yrs 会建一个空的根类型（不会编进更新，但没必要）。
        return None;
    }
    read_row(table, key)
}

/// 写一行：给出的字段覆盖上去，`Any::Undefined` 的字段删掉，别的字段不动；删掉的行写了就恢复。
pub fn upsert_row(table: &Doc, key: &str, fields: &[(&str, Any)]) {
    let m = table.get_or_insert_map(key);
    let mut txn = table.transact_mut();
    for (k, v) in fields {
        if matches!(v, Any::Undefined) {
            m.remove(&mut txn, k);
        } else {
            m.insert(&mut txn, *k, v.clone());
        }
    }
    m.remove(&mut txn, DELETED);
}

/// 删一行：除了主键字段都去掉，标上 `$$DELETED`（和 AFFiNE 一样，别的端看得到这次删除）。
pub fn delete_row(table: &Doc, key: &str, key_field: &str) -> bool {
    if row(table, key).is_none() {
        return false;
    }
    let m = table.get_or_insert_map(key);
    let mut txn = table.transact_mut();
    let names: Vec<String> = m.keys(&txn).map(str::to_owned).collect();
    for k in names {
        if k != key_field {
            m.remove(&mut txn, &k);
        }
    }
    m.insert(&mut txn, DELETED, true);
    true
}

// ---------------------------------------------------------------- 文件夹树（表 `folders`）

/// 文件夹表的名字。
pub const FOLDERS: &str = "folders";
/// 文档属性表的名字。
pub const DOC_PROPERTIES: &str = "docProperties";

/// 文件夹树里一个节点的类型。根上只能放文件夹；文件夹里可以放文件夹，也可以链到文档、标签、合集。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Folder,
    Doc,
    Tag,
    Collection,
    Other(String),
}

impl NodeKind {
    fn as_str(&self) -> &str {
        match self {
            Self::Folder => "folder",
            Self::Doc => "doc",
            Self::Tag => "tag",
            Self::Collection => "collection",
            Self::Other(s) => s,
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "folder" => Self::Folder,
            "doc" => Self::Doc,
            "tag" => Self::Tag,
            "collection" => Self::Collection,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// 文件夹树的一个节点。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderNode {
    pub id: String,
    /// 放在哪个文件夹里；`None` 是根上。
    pub parent_id: Option<String>,
    pub kind: NodeKind,
    /// 文件夹是名字；链接是目标的 id（文档 id、标签 id……）。
    pub data: String,
    /// 排序键（[`crate::order`]）。
    pub index: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FolderError {
    #[error("找不到节点 {0}")]
    NotFound(String),
    #[error("{0} 不是文件夹")]
    NotFolder(String),
    #[error("根上只能放文件夹")]
    RootOnlyFolders,
    #[error("不能把节点挪到它自己或它下面")]
    Cycle,
    #[error(transparent)]
    Order(#[from] OrderError),
}

/// 读出整棵树（没删掉的节点）。
pub fn folders(table: &Doc) -> Vec<FolderNode> {
    rows(table)
        .into_iter()
        .filter_map(|r| {
            Some(FolderNode {
                id: r.str("id").unwrap_or(&r.key).to_owned(),
                parent_id: r.str("parentId").map(str::to_owned),
                kind: NodeKind::parse(r.str("type")?),
                data: r.str("data").unwrap_or_default().to_owned(),
                index: r.str("index").unwrap_or_default().to_owned(),
            })
        })
        .collect()
}

/// 某个文件夹（`None` 是根）下面的节点，按排序键排好。
pub fn children<'a>(nodes: &'a [FolderNode], parent: Option<&str>) -> Vec<&'a FolderNode> {
    let mut out: Vec<&FolderNode> = nodes
        .iter()
        .filter(|n| n.parent_id.as_deref() == parent)
        .collect();
    out.sort_by(|a, b| a.index.cmp(&b.index).then(a.id.cmp(&b.id)));
    out
}

fn parent_ok(nodes: &[FolderNode], parent: Option<&str>) -> Result<(), FolderError> {
    if let Some(p) = parent {
        match nodes.iter().find(|n| n.id == p) {
            None => return Err(FolderError::NotFound(p.to_owned())),
            Some(n) if n.kind != NodeKind::Folder => {
                return Err(FolderError::NotFolder(p.to_owned()));
            }
            _ => {}
        }
    }
    Ok(())
}

/// 放在某个文件夹最后面用的排序键。
fn index_at_end(nodes: &[FolderNode], parent: Option<&str>) -> Result<String, FolderError> {
    let last = children(nodes, parent).last().map(|n| n.index.clone());
    Ok(order::key_between_random(last.as_deref(), None)?)
}

fn write_node(table: &Doc, n: &FolderNode) {
    let parent = match &n.parent_id {
        Some(p) => Any::from(p.as_str()),
        None => Any::Null,
    };
    upsert_row(
        table,
        &n.id,
        &[
            ("id", Any::from(n.id.as_str())),
            ("parentId", parent),
            ("type", Any::from(n.kind.as_str())),
            ("data", Any::from(n.data.as_str())),
            ("index", Any::from(n.index.as_str())),
        ],
    );
}

/// 建一个文件夹，放在 `parent`（`None` 是根）的最后面，返回它的 id。
pub fn create_folder(
    table: &Doc,
    parent: Option<&str>,
    name: &str,
    ids: &mut impl Ids,
) -> Result<String, FolderError> {
    let nodes = folders(table);
    parent_ok(&nodes, parent)?;
    let node = FolderNode {
        id: ids.next_id(),
        parent_id: parent.map(str::to_owned),
        kind: NodeKind::Folder,
        data: name.to_owned(),
        index: index_at_end(&nodes, parent)?,
    };
    write_node(table, &node);
    Ok(node.id)
}

/// 在文件夹里放一个链接（文档、标签、合集），放在最后面，返回链接节点的 id。
pub fn create_link(
    table: &Doc,
    folder: &str,
    kind: NodeKind,
    target: &str,
    ids: &mut impl Ids,
) -> Result<String, FolderError> {
    if kind == NodeKind::Folder {
        return Err(FolderError::NotFolder(target.to_owned()));
    }
    let nodes = folders(table);
    parent_ok(&nodes, Some(folder))?;
    let node = FolderNode {
        id: ids.next_id(),
        parent_id: Some(folder.to_owned()),
        kind,
        data: target.to_owned(),
        index: index_at_end(&nodes, Some(folder))?,
    };
    write_node(table, &node);
    Ok(node.id)
}

/// 改文件夹的名字。
pub fn rename_folder(table: &Doc, id: &str, name: &str) -> Result<(), FolderError> {
    let nodes = folders(table);
    let n = nodes
        .iter()
        .find(|n| n.id == id)
        .ok_or_else(|| FolderError::NotFound(id.to_owned()))?;
    if n.kind != NodeKind::Folder {
        return Err(FolderError::NotFolder(id.to_owned()));
    }
    upsert_row(table, id, &[("data", Any::from(name))]);
    Ok(())
}

/// 把一个节点挪到 `parent`（`None` 是根）的最后面。
pub fn move_node(table: &Doc, id: &str, parent: Option<&str>) -> Result<(), FolderError> {
    let nodes = folders(table);
    let n = nodes
        .iter()
        .find(|n| n.id == id)
        .ok_or_else(|| FolderError::NotFound(id.to_owned()))?;
    match parent {
        None if n.kind != NodeKind::Folder => return Err(FolderError::RootOnlyFolders),
        Some(p) => {
            parent_ok(&nodes, Some(p))?;
            // 往上找，碰到自己就是挪进了自己下面。
            let mut cur = Some(p.to_owned());
            while let Some(c) = cur {
                if c == id {
                    return Err(FolderError::Cycle);
                }
                cur = nodes
                    .iter()
                    .find(|x| x.id == c)
                    .and_then(|x| x.parent_id.clone());
            }
        }
        None => {}
    }
    let mut moved = n.clone();
    moved.parent_id = parent.map(str::to_owned);
    let others: Vec<FolderNode> = nodes.iter().filter(|x| x.id != id).cloned().collect();
    moved.index = index_at_end(&others, parent)?;
    write_node(table, &moved);
    Ok(())
}

/// 删一个节点；删文件夹时连同下面所有的节点一起删（链接指向的文档本身不动）。返回删掉了几个。
pub fn remove_node(table: &Doc, id: &str) -> Result<usize, FolderError> {
    let nodes = folders(table);
    if !nodes.iter().any(|n| n.id == id) {
        return Err(FolderError::NotFound(id.to_owned()));
    }
    let mut stack = vec![id.to_owned()];
    let mut removed = 0;
    while let Some(cur) = stack.pop() {
        stack.extend(
            nodes
                .iter()
                .filter(|n| n.parent_id.as_deref() == Some(cur.as_str()))
                .map(|n| n.id.clone()),
        );
        if delete_row(table, &cur, "id") {
            removed += 1;
        }
    }
    Ok(removed)
}

// ---------------------------------------------------------------- 文档属性（表 `docProperties`）

/// 一篇文档的属性：主键是文档 id。AFFiNE 认的字段有 `primaryMode`（`page` 或 `edgeless`）、
/// `createdBy`、`updatedBy`、`isTemplate`、`journal`……，自定义属性是 `custom:<属性 id>`。
pub fn doc_properties(table: &Doc, doc_id: &str) -> Option<BTreeMap<String, Any>> {
    row(table, doc_id).map(|mut r| {
        r.fields.remove("id");
        r.fields
    })
}

/// 改一篇文档的属性：给出的字段覆盖上去，`Any::Undefined` 的删掉。
pub fn set_doc_properties(table: &Doc, doc_id: &str, fields: &[(&str, Any)]) {
    let mut all: Vec<(&str, Any)> = vec![("id", Any::from(doc_id))];
    all.extend(fields.iter().cloned());
    upsert_row(table, doc_id, &all);
}
