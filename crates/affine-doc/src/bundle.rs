//! AFFiNE 导出的工作区包（`.bs.zip`，“导出 → 快照”）的读写。
//!
//! 包里的样子照 BlockSuite 的导出（只读了设计，没复制代码）：
//!
//! - 每篇文档一个 `<标题>-<文档 id>.snapshot.json`，内容是 [`crate::snapshot`] 的快照，两个空格缩进；
//! - 图片和附件放在 `assets/<blobId>.<扩展名>`，`blobId` 就是块里 `sourceId` 写的那个；
//!   读的时候也认外面多套一层目录的 `<目录>/assets/...`；
//! - 读的时候跳过 `__MACOSX`、`.DS_Store` 和 `info.json`。
//!
//! 包里只有文档和附件，没有文件夹树、文档属性这些工作区的表，AFFiNE 自己导出的也没有。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Cursor, Read, Write};

use serde_json::Value;
use yrs::Doc;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::doc::Ids;
use crate::snapshot::{SnapshotError, TEXT_MARK, from_snapshot, to_snapshot};
use crate::workspace::{self, DocMeta};

/// 读包时解压出来的总大小上限（1 GiB），防止压缩炸弹。
pub const MAX_UNPACKED: u64 = 1 << 30;

const ASSETS: &str = "assets/";
const SNAPSHOT_SUFFIX: &str = ".snapshot.json";
/// 块里带附件的：图片和附件，附件的 id 在 `props.sourceId`。
const ASSET_FLAVOURS: [&str; 2] = ["affine:image", "affine:attachment"];
/// 块里指向另一篇文档的：`props.pageId`。
const DOC_LINK_FLAVOURS: [&str; 2] = ["affine:embed-linked-doc", "affine:embed-synced-doc"];
const SURFACE_TEXT_MARK: &str = "affine:surface:text";

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("zip 包读写出错：{0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("zip 包读写出错：{0}")]
    Io(#[from] std::io::Error),
    #[error("{0} 不是合法的 JSON：{1}")]
    Json(String, serde_json::Error),
    #[error("{0}：{1}")]
    Snapshot(String, SnapshotError),
    #[error("解压出来超过 {MAX_UNPACKED} 字节")]
    TooLarge,
    #[error("包里有两篇文档的 id 都是 {0}")]
    DuplicateDoc(String),
}

/// 一个附件：扩展名（不带点）和内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub ext: String,
    pub bytes: Vec<u8>,
}

/// 解开的包：每篇文档的快照，和按 `blobId` 放的附件。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bundle {
    pub docs: Vec<Value>,
    pub assets: BTreeMap<String, Asset>,
}

/// 按 MIME 类型给附件取扩展名，取不到用类型的后半截，空的用 `blob`（和 BlockSuite 一样）。
/// 只列了常见的几种。
pub fn ext_for_mime(mime: &str) -> &str {
    match mime {
        "" => "blob",
        "image/jpeg" => "jpeg",
        "image/svg+xml" => "svg",
        "image/vnd.microsoft.icon" => "ico",
        "text/plain" => "txt",
        "text/javascript" => "js",
        "application/octet-stream" => "bin",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.ms-powerpoint" => "ppt",
        "audio/mpeg" => "mp3",
        "video/quicktime" => "mov",
        other => other.rsplit('/').next().unwrap_or("blob"),
    }
}

fn walk_blocks<'a>(b: &'a Value, f: &mut impl FnMut(&'a Value)) {
    f(b);
    if let Some(Value::Array(children)) = b.get("children") {
        for c in children {
            walk_blocks(c, f);
        }
    }
}

/// 一篇快照里用到的附件 id（图片和附件块的 `sourceId`），按出现的先后，不重复。
pub fn asset_ids(snapshot: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    if let Some(root) = snapshot.get("blocks") {
        walk_blocks(root, &mut |b| {
            let flavour = b.get("flavour").and_then(Value::as_str).unwrap_or_default();
            if !ASSET_FLAVOURS.contains(&flavour) {
                return;
            }
            if let Some(id) = b.pointer("/props/sourceId").and_then(Value::as_str)
                && !id.is_empty()
                && seen.insert(id.to_owned())
            {
                out.push(id.to_owned());
            }
        });
    }
    out
}

/// 把快照里的文档 id 按 `map` 换掉：`meta.id`、文字里的行内引用（`reference.pageId`）、
/// 嵌入文档块的 `pageId`。不在 `map` 里的不动。块 id 不换：每篇文档的块 id 只在本篇里不重复。
pub fn remap_doc_ids(snapshot: &mut Value, map: &HashMap<String, String>) {
    fn swap(v: &mut Value, map: &HashMap<String, String>) {
        if let Some(new) = v.as_str().and_then(|s| map.get(s)) {
            *v = Value::String(new.clone());
        }
    }
    fn walk(v: &mut Value, map: &HashMap<String, String>) {
        match v {
            Value::Object(o) => {
                let is_text = o.contains_key(TEXT_MARK) || o.contains_key(SURFACE_TEXT_MARK);
                if is_text && let Some(Value::Array(delta)) = o.get_mut("delta") {
                    for op in delta {
                        if let Some(id) = op.pointer_mut("/attributes/reference/pageId") {
                            swap(id, map);
                        }
                    }
                    return;
                }
                let links_doc = o
                    .get("flavour")
                    .and_then(Value::as_str)
                    .is_some_and(|f| DOC_LINK_FLAVOURS.contains(&f));
                if links_doc && let Some(id) = o.get_mut("props").and_then(|p| p.get_mut("pageId"))
                {
                    swap(id, map);
                }
                for x in o.values_mut() {
                    walk(x, map);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| walk(x, map)),
            _ => {}
        }
    }
    if let Some(id) = snapshot.pointer_mut("/meta/id") {
        swap(id, map);
    }
    if let Some(blocks) = snapshot.get_mut("blocks") {
        walk(blocks, map);
    }
}

fn file_name(snapshot: &Value) -> String {
    let meta = snapshot.get("meta");
    let field = |k: &str| {
        meta.and_then(|m| m.get(k))
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    let title = match field("title") {
        "" => "untitled".to_owned(),
        // 标题里的斜杠会让 zip 里多出一层目录。
        t => t.replace(['/', '\\'], "_"),
    };
    format!("{title}-{}{SNAPSHOT_SUFFIX}", field("id"))
}

/// 写成 zip 包。文档按 `docs` 的先后，附件按 `blobId` 排。
pub fn write_bundle(bundle: &Bundle) -> Result<Vec<u8>, BundleError> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut ids = HashSet::new();
    for snap in &bundle.docs {
        let id = snap
            .pointer("/meta/id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !ids.insert(id.to_owned()) {
            return Err(BundleError::DuplicateDoc(id.to_owned()));
        }
        let name = file_name(snap);
        let json =
            serde_json::to_vec_pretty(snap).map_err(|e| BundleError::Json(name.clone(), e))?;
        zip.start_file(name, opts)?;
        zip.write_all(&json)?;
    }
    for (id, asset) in &bundle.assets {
        zip.start_file(format!("{ASSETS}{id}.{}", asset.ext), opts)?;
        zip.write_all(&asset.bytes)?;
    }
    Ok(zip.finish()?.into_inner())
}

/// 解开 zip 包。快照只检查是不是 JSON、`type` 是不是 `page`，写进 Yjs 时再细查。
pub fn read_bundle(bytes: &[u8]) -> Result<Bundle, BundleError> {
    let mut zip = ZipArchive::new(Cursor::new(bytes))?;
    let mut out = Bundle::default();
    let mut budget = MAX_UNPACKED;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i)?;
        if file.is_dir() {
            continue;
        }
        let path = file.name()?.into_owned();
        if path.contains("MACOSX") || path.contains("DS_Store") || path == "info.json" {
            continue;
        }
        // 有的包（比如 AFFiNE 的白板模板）外面多套了一层目录：`<名字>/assets/...`。
        let asset_name = path
            .strip_prefix(ASSETS)
            .or_else(|| path.split_once("/assets/").map(|(_, n)| n))
            .map(str::to_owned);
        if asset_name.is_none() && !path.ends_with(SNAPSHOT_SUFFIX) {
            continue;
        }
        // 按实际解出来的字节数算，不信 zip 里写的大小。
        let mut buf = Vec::new();
        (&mut file).take(budget + 1).read_to_end(&mut buf)?;
        if buf.len() as u64 > budget {
            return Err(BundleError::TooLarge);
        }
        budget -= buf.len() as u64;
        if let Some(name) = asset_name {
            let (id, ext) = name.rsplit_once('.').unwrap_or((&name, ""));
            out.assets.insert(
                id.to_owned(),
                Asset {
                    ext: ext.to_owned(),
                    bytes: buf,
                },
            );
        } else {
            let snap: Value =
                serde_json::from_slice(&buf).map_err(|e| BundleError::Json(path.clone(), e))?;
            if snap.get("type").and_then(Value::as_str) != Some("page") {
                return Err(BundleError::Snapshot(
                    path,
                    SnapshotError::Format("type 不是 page".into()),
                ));
            }
            out.docs.push(snap);
        }
    }
    Ok(out)
}

/// 导出的结果：包的字节，和找不到内容的附件 id（BlockSuite 也是记下来、照样导出）。
#[derive(Debug)]
pub struct Exported {
    pub zip: Vec<u8>,
    pub missing_assets: Vec<String>,
}

/// 把几篇文档导出成一个包。`asset` 按 `blobId` 取附件内容，取不到返回 `None`。
pub fn export_docs(
    docs: &[(&Doc, &DocMeta)],
    mut asset: impl FnMut(&str) -> Option<Asset>,
) -> Result<Exported, BundleError> {
    let mut bundle = Bundle::default();
    let mut missing = Vec::new();
    for (doc, meta) in docs {
        let snap = to_snapshot(doc, meta).map_err(|e| BundleError::Snapshot(meta.id.clone(), e))?;
        for id in asset_ids(&snap) {
            if bundle.assets.contains_key(&id) || missing.contains(&id) {
                continue;
            }
            match asset(&id) {
                Some(a) => {
                    bundle.assets.insert(id, a);
                }
                None => missing.push(id),
            }
        }
        bundle.docs.push(snap);
    }
    Ok(Exported {
        zip: write_bundle(&bundle)?,
        missing_assets: missing,
    })
}

/// 导进来的一篇文档：新的 `meta`（id 换过了）和写好的 Yjs 文档。
#[derive(Debug)]
pub struct Imported {
    pub meta: DocMeta,
    pub doc: Doc,
}

/// 把包里的文档导进工作区：每篇换一个新的文档 id（文档之间的引用跟着换），写成新的 Yjs 文档，
/// 全部写对了才记进根文档的文档列表。附件不在这里处理：调用方把 `bundle.assets` 按 `blobId`
/// 传到 AFFiNE 的附件存储。
///
/// 和 BlockSuite 导入的不同：块 id 不换（每篇文档是新的，块 id 不会撞）；标签照快照里的 id 写，
/// 工作区里没有这个标签就显示不出来。
pub fn import_docs(
    root: &Doc,
    bundle: &Bundle,
    ids: &mut impl Ids,
) -> Result<Vec<Imported>, BundleError> {
    let mut map = HashMap::new();
    for snap in &bundle.docs {
        let old = snap
            .pointer("/meta/id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if map.contains_key(&old) {
            return Err(BundleError::DuplicateDoc(old));
        }
        map.insert(old, ids.next_id());
    }
    let mut out = Vec::new();
    for snap in &bundle.docs {
        let mut snap = snap.clone();
        remap_doc_ids(&mut snap, &map);
        let doc = Doc::new();
        let name = file_name(&snap);
        let meta = from_snapshot(&doc, &snap).map_err(|e| BundleError::Snapshot(name, e))?;
        out.push(Imported { meta, doc });
    }
    for d in &out {
        workspace::upsert_doc_meta(root, &d.meta);
    }
    Ok(out)
}
