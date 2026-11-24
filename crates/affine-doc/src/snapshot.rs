//! BlockSuite 的文档快照（AFFiNE 导出的 `.snapshot.json`）和 Yjs 文档互相转换。
//!
//! 快照是一篇文档的 JSON：`{type: "page", meta: {id, title, createDate, tags}, blocks: <页面块>}`，每个块
//! `{type: "block", id, flavour, version, props, children}`。属性里的特殊值有两种标记：
//!
//! - 带格式的文字：`{"$blocksuite:internal:text$": true, "delta": [{insert, attributes?}, ...]}`，在 Yjs 里是 Y.Text；
//! - 原样存的值（BlockSuite 的 `Boxed`）：`{"$blocksuite:internal:native$": true, "value": ...}`，在 Yjs 里是
//!   一个 Y.Map `{type: "$blocksuite:internal:native$", value: ...}`。
//!
//! 别的对象在 Yjs 里是 Y.Map，数组是 Y.Array（BlockSuite 写属性时整个往下转）。两种块另有规矩：
//!
//! - `affine:surface` 的 `elements`：快照里是 `{元素 id: {字段: 值}}`，字段的值里文字标成
//!   `{"affine:surface:text": true, delta}`、Y.Map 标成 `{"affine:surface:ymap": true, json}`；Yjs 里是一个
//!   Boxed，`value` 是元素 id 到元素 Y.Map 的 Y.Map；
//! - `affine:table` 这类拍平存的块：快照里属性是嵌套的，Yjs 里是 `prop:a.b.c` 这样的键。
//!
//! 依据是 AFFiNE 仓库里 MIT 部分的 `blocksuite/framework/store/src/transformer`、
//! `blocksuite/framework/store/src/reactive/boxed.ts`、`blocksuite/affine/blocks/surface/src/surface-transformer.ts`，
//! 只读了设计，没复制代码。附件（`sourceId` 指向的文件）不在快照里，整个工作区的导出（zip）在下一步。

use std::collections::{BTreeMap, HashSet};

use serde_json::{Map as JsonMap, Value, json};
use yrs::types::Attrs;
use yrs::updates::decoder::Decode;
use yrs::{
    Any, Array, ArrayPrelim, ArrayRef, Doc, Map, MapPrelim, MapRef, ReadTxn, StateVector, Text,
    TextPrelim, TextRef, Transact, TransactionMut, Update,
};

use crate::schema::{self, Delta, Prop, RawBlock};
use crate::workspace::DocMeta;

/// 带格式文字的标记。
pub const TEXT_MARK: &str = "$blocksuite:internal:text$";
/// 原样存的值的标记。
pub const NATIVE_MARK: &str = "$blocksuite:internal:native$";
const SURFACE_TEXT_MARK: &str = "affine:surface:text";
const SURFACE_YMAP_MARK: &str = "affine:surface:ymap";
const SURFACE: &str = "affine:surface";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SnapshotError {
    #[error("文档里没有 affine:page 块")]
    NoPage,
    #[error("文档里已经有块了，不能把快照写进去")]
    NotEmpty,
    #[error("快照格式不对：{0}")]
    Format(String),
    #[error("快照里块 id {0} 重复")]
    DuplicateId(String),
}

fn bad(what: impl Into<String>) -> SnapshotError {
    SnapshotError::Format(what.into())
}

// ---------------------------------------------------------------- Yjs → 快照

fn any_json(a: &Any) -> Value {
    match a {
        Any::Undefined => Value::Null,
        other => serde_json::to_value(other).unwrap_or(Value::Null),
    }
}

fn attrs_json(attrs: &BTreeMap<String, Any>) -> Option<Value> {
    (!attrs.is_empty()).then(|| {
        Value::Object(
            attrs
                .iter()
                .map(|(k, v)| (k.clone(), any_json(v)))
                .collect(),
        )
    })
}

fn delta_json(delta: &[Delta]) -> Value {
    Value::Array(
        delta
            .iter()
            .map(|d| {
                let (insert, attrs) = match d {
                    Delta::Text { text, attrs } => (Value::String(text.clone()), attrs),
                    Delta::Embed { value, attrs } => (any_json(value), attrs),
                };
                let mut o = JsonMap::new();
                o.insert("insert".into(), insert);
                if let Some(a) = attrs_json(attrs) {
                    o.insert("attributes".into(), a);
                }
                Value::Object(o)
            })
            .collect(),
    )
}

fn is_boxed(m: &BTreeMap<String, Prop>) -> bool {
    matches!(m.get("type"), Some(Prop::Value(Any::String(s))) if &**s == NATIVE_MARK)
}

fn prop_json(p: &Prop) -> Value {
    match p {
        Prop::Text(d) => json!({ TEXT_MARK: true, "delta": delta_json(d) }),
        Prop::Map(m) if is_boxed(m) => json!({
            NATIVE_MARK: true,
            "value": m.get("value").map(prop_json).unwrap_or(Value::Null),
        }),
        Prop::Map(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), prop_json(v))).collect()),
        Prop::List(l) => Value::Array(l.iter().map(prop_json).collect()),
        Prop::Value(a) => any_json(a),
    }
}

/// 白板元素里的一个字段。
fn surface_field_json(p: &Prop) -> Value {
    match p {
        Prop::Text(d) => json!({ SURFACE_TEXT_MARK: true, "delta": delta_json(d) }),
        Prop::Map(m) => json!({
            SURFACE_YMAP_MARK: true,
            "json": Value::Object(m.iter().map(|(k, v)| (k.clone(), prop_json(v))).collect()),
        }),
        other => prop_json(other),
    }
}

fn surface_elements_json(p: Option<&Prop>) -> Value {
    let inner = match p {
        Some(Prop::Map(m)) if is_boxed(m) => m.get("value"),
        other => other,
    };
    let Some(Prop::Map(elements)) = inner else {
        return json!({});
    };
    Value::Object(
        elements
            .iter()
            .map(|(id, el)| {
                let v = match el {
                    Prop::Map(fields) => Value::Object(
                        fields
                            .iter()
                            .map(|(k, v)| (k.clone(), surface_field_json(v)))
                            .collect(),
                    ),
                    other => prop_json(other),
                };
                (id.clone(), v)
            })
            .collect(),
    )
}

fn block_json(b: &RawBlock) -> Value {
    let props: JsonMap<String, Value> = if b.flavour == SURFACE {
        let mut m = JsonMap::new();
        m.insert("elements".into(), surface_elements_json(b.prop("elements")));
        m
    } else {
        b.props
            .iter()
            .map(|(k, v)| (k.clone(), prop_json(v)))
            .collect()
    };
    let mut o = JsonMap::new();
    o.insert("type".into(), "block".into());
    o.insert("id".into(), b.id.clone().into());
    o.insert("flavour".into(), b.flavour.clone().into());
    if let Some(v) = b.version {
        o.insert("version".into(), v.into());
    }
    o.insert("props".into(), Value::Object(props));
    o.insert(
        "children".into(),
        Value::Array(b.children.iter().map(block_json).collect()),
    );
    Value::Object(o)
}

/// 把一篇文档写成快照。`meta` 是根文档里这篇的那一项（[`crate::workspace::read_meta`]）；
/// 标题为空时用页面块的标题。
pub fn to_snapshot(doc: &Doc, meta: &DocMeta) -> Result<Value, SnapshotError> {
    let tree = schema::read_tree(doc).ok_or(SnapshotError::NoPage)?;
    let title = if meta.title.is_empty() {
        tree.prop("title")
            .and_then(Prop::plain_text)
            .unwrap_or_default()
    } else {
        meta.title.clone()
    };
    Ok(json!({
        "type": "page",
        "meta": {
            "id": meta.id,
            "title": title,
            "createDate": number_json(meta.create_date.unwrap_or(0.0)),
            "tags": meta.tags,
        },
        "blocks": block_json(&tree),
    }))
}

/// 整数写成整数（JSON 里 `1744684776321` 和 `1744684776321.0` 不一样）。
fn number_json(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        Value::from(n as i64)
    } else {
        Value::from(n)
    }
}

// ---------------------------------------------------------------- 快照 → Yjs

fn json_any(v: &Value) -> Any {
    serde_json::from_value(v.clone()).unwrap_or(Any::Null)
}

fn json_attrs(v: Option<&Value>) -> Result<Option<Attrs>, SnapshotError> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(o)) if o.is_empty() => Ok(None),
        Some(Value::Object(o)) => Ok(Some(
            o.iter()
                .map(|(k, v)| (k.as_str().into(), json_any(v)))
                .collect(),
        )),
        Some(_) => Err(bad("文字的 attributes 不是对象")),
    }
}

/// 按快照里的 delta 往一个空的 Y.Text 里写。
fn write_delta(txn: &mut TransactionMut, t: &TextRef, delta: &Value) -> Result<(), SnapshotError> {
    let Value::Array(items) = delta else {
        return Err(bad("delta 不是数组"));
    };
    // Yjs 里接着往后插、不带属性的话会沿用前一段的格式，所以前一段有、这一段没有的属性要明确写成 null。
    let mut prev: Vec<std::sync::Arc<str>> = Vec::new();
    for item in items {
        let mut attrs = json_attrs(item.get("attributes"))?.unwrap_or_default();
        let keys: Vec<std::sync::Arc<str>> = attrs.keys().cloned().collect();
        for k in prev.drain(..) {
            attrs.entry(k).or_insert(Any::Null);
        }
        prev = keys;
        let at = t.len(txn);
        match item.get("insert") {
            Some(Value::String(s)) => t.insert_with_attributes(txn, at, s, attrs),
            Some(other) => {
                t.insert_embed_with_attributes(txn, at, json_any(other), attrs);
            }
            None => return Err(bad("delta 里有一项没有 insert")),
        }
    }
    Ok(())
}

fn text_delta<'a>(o: &'a JsonMap<String, Value>, mark: &str) -> Option<&'a Value> {
    matches!(o.get(mark), Some(Value::Bool(true))).then(|| o.get("delta").unwrap_or(&Value::Null))
}

/// 往 Y.Map 里写一个属性值（BlockSuite 的规矩：对象变 Y.Map，数组变 Y.Array）。
fn put_in_map(
    txn: &mut TransactionMut,
    m: &MapRef,
    key: &str,
    v: &Value,
) -> Result<(), SnapshotError> {
    match v {
        Value::Object(o) => {
            if let Some(delta) = text_delta(o, TEXT_MARK) {
                let t: TextRef = m.insert(txn, key, TextPrelim::new(""));
                write_delta(txn, &t, delta)?;
            } else if matches!(o.get(NATIVE_MARK), Some(Value::Bool(true))) {
                let boxed: MapRef = m.insert(txn, key, MapPrelim::default());
                boxed.insert(txn, "type", NATIVE_MARK);
                boxed.insert(
                    txn,
                    "value",
                    json_any(o.get("value").unwrap_or(&Value::Null)),
                );
            } else {
                let inner: MapRef = m.insert(txn, key, MapPrelim::default());
                for (k, v) in o {
                    put_in_map(txn, &inner, k, v)?;
                }
            }
        }
        Value::Array(items) => {
            let a: ArrayRef = m.insert(txn, key, ArrayPrelim::default());
            for v in items {
                push_in_array(txn, &a, v)?;
            }
        }
        other => {
            m.insert(txn, key, json_any(other));
        }
    }
    Ok(())
}

fn push_in_array(txn: &mut TransactionMut, a: &ArrayRef, v: &Value) -> Result<(), SnapshotError> {
    match v {
        Value::Object(o) => {
            if let Some(delta) = text_delta(o, TEXT_MARK) {
                let t: TextRef = a.push_back(txn, TextPrelim::new(""));
                write_delta(txn, &t, delta)?;
            } else {
                let inner: MapRef = a.push_back(txn, MapPrelim::default());
                for (k, v) in o {
                    put_in_map(txn, &inner, k, v)?;
                }
            }
        }
        Value::Array(items) => {
            let inner: ArrayRef = a.push_back(txn, ArrayPrelim::default());
            for v in items {
                push_in_array(txn, &inner, v)?;
            }
        }
        other => {
            a.push_back(txn, json_any(other));
        }
    }
    Ok(())
}

/// 白板的 `elements`：一个 Boxed，`value` 是元素 id → 元素 Y.Map。
fn put_surface_elements(
    txn: &mut TransactionMut,
    block: &MapRef,
    elements: Option<&Value>,
) -> Result<(), SnapshotError> {
    let boxed: MapRef = block.insert(txn, "prop:elements", MapPrelim::default());
    boxed.insert(txn, "type", NATIVE_MARK);
    let all: MapRef = boxed.insert(txn, "value", MapPrelim::default());
    let Some(Value::Object(elements)) = elements else {
        return Ok(());
    };
    for (id, el) in elements {
        let Value::Object(fields) = el else {
            return Err(bad(format!("白板元素 {id} 不是对象")));
        };
        let m: MapRef = all.insert(txn, id.as_str(), MapPrelim::default());
        for (k, v) in fields {
            match v {
                Value::Object(o) if text_delta(o, SURFACE_TEXT_MARK).is_some() => {
                    let t: TextRef = m.insert(txn, k.as_str(), TextPrelim::new(""));
                    write_delta(
                        txn,
                        &t,
                        text_delta(o, SURFACE_TEXT_MARK).unwrap_or(&Value::Null),
                    )?;
                }
                Value::Object(o) if matches!(o.get(SURFACE_YMAP_MARK), Some(Value::Bool(true))) => {
                    let inner: MapRef = m.insert(txn, k.as_str(), MapPrelim::default());
                    if let Some(Value::Object(j)) = o.get("json") {
                        for (jk, jv) in j {
                            inner.insert(txn, jk.as_str(), json_any(jv));
                        }
                    }
                }
                // 元素的别的字段 BlockSuite 原样存（不转成 Y.Map）。
                other => {
                    m.insert(txn, k.as_str(), json_any(other));
                }
            }
        }
    }
    Ok(())
}

/// 拍平存的块：嵌套的对象拍成 `a.b.c`，文字和别的值是叶子。
fn flatten<'a>(prefix: &str, v: &'a Value, out: &mut Vec<(String, &'a Value)>) {
    match v {
        Value::Object(o) if text_delta(o, TEXT_MARK).is_none() && !o.is_empty() => {
            for (k, v) in o {
                flatten(&format!("{prefix}.{k}"), v, out);
            }
        }
        other => out.push((prefix.to_owned(), other)),
    }
}

struct Writer<'a> {
    blocks: &'a MapRef,
    seen: HashSet<String>,
}

impl Writer<'_> {
    fn block(
        &mut self,
        txn: &mut TransactionMut,
        b: &Value,
        depth: usize,
    ) -> Result<String, SnapshotError> {
        if depth > 64 {
            return Err(bad("块嵌套太深"));
        }
        let id = b
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("块没有 id"))?
            .to_owned();
        if !self.seen.insert(id.clone()) {
            return Err(SnapshotError::DuplicateId(id));
        }
        let flavour = b
            .get("flavour")
            .and_then(Value::as_str)
            .ok_or_else(|| bad(format!("块 {id} 没有 flavour")))?
            .to_owned();
        let version = b
            .get("version")
            .and_then(Value::as_i64)
            .or_else(|| schema::flavour(&flavour).map(|f| f.version));
        let m: MapRef = self.blocks.insert(txn, id.as_str(), MapPrelim::default());
        m.insert(txn, "sys:id", id.as_str());
        m.insert(txn, "sys:flavour", flavour.as_str());
        if let Some(v) = version {
            m.insert(txn, "sys:version", Any::from(v));
        }
        let props = match b.get("props") {
            None | Some(Value::Null) => JsonMap::new(),
            Some(Value::Object(o)) => o.clone(),
            Some(_) => return Err(bad(format!("块 {id} 的 props 不是对象"))),
        };
        if flavour == SURFACE {
            put_surface_elements(txn, &m, props.get("elements"))?;
        } else if schema::flavour(&flavour).is_some_and(|f| f.flat_data) {
            let mut flat = Vec::new();
            for (k, v) in &props {
                flatten(k, v, &mut flat);
            }
            for (k, v) in flat {
                put_in_map(txn, &m, &format!("prop:{k}"), v)?;
            }
        } else {
            for (k, v) in &props {
                put_in_map(txn, &m, &format!("prop:{k}"), v)?;
            }
        }
        let children = match b.get("children") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a.clone(),
            Some(_) => return Err(bad(format!("块 {id} 的 children 不是数组"))),
        };
        let mut ids = Vec::with_capacity(children.len());
        for c in &children {
            ids.push(self.block(txn, c, depth + 1)?);
        }
        let arr: ArrayRef = m.insert(txn, "sys:children", ArrayPrelim::default());
        for c in ids {
            arr.push_back(txn, c.as_str());
        }
        Ok(id)
    }
}

/// 把快照写进一篇空的文档，返回快照里的 `meta`（写进根文档的文档列表要另调
/// [`crate::workspace::upsert_doc_meta`]）。
pub fn from_snapshot(doc: &Doc, snapshot: &Value) -> Result<DocMeta, SnapshotError> {
    if snapshot.get("type").and_then(Value::as_str) != Some("page") {
        return Err(bad("type 不是 page"));
    }
    let meta = snapshot.get("meta").ok_or_else(|| bad("没有 meta"))?;
    let root = snapshot.get("blocks").ok_or_else(|| bad("没有 blocks"))?;
    if root.get("flavour").and_then(Value::as_str) != Some("affine:page") {
        return Err(SnapshotError::NoPage);
    }
    {
        let blocks = doc.get_or_insert_map("blocks");
        let txn = doc.transact();
        if blocks.len(&txn) > 0 {
            return Err(SnapshotError::NotEmpty);
        }
    }
    // 先写进一篇临时的文档，整篇都对了再作为一个更新应用过去：中途出错时目标文档不会只写了一半。
    let scratch = Doc::new();
    {
        let blocks = scratch.get_or_insert_map("blocks");
        let mut txn = scratch.transact_mut();
        let mut w = Writer {
            blocks: &blocks,
            seen: HashSet::new(),
        };
        w.block(&mut txn, root, 0)?;
    }
    let update = scratch
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    doc.transact_mut()
        .apply_update(Update::decode_v1(&update).map_err(|e| bad(e.to_string()))?)
        .map_err(|e| bad(e.to_string()))?;
    let id = meta
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("meta 里没有 id"))?;
    let mut out = DocMeta::new(
        id,
        meta.get("title")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        meta.get("createDate")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
    );
    if let Some(Value::Array(tags)) = meta.get("tags") {
        out.tags = tags
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
    }
    Ok(out)
}
