use serde_json::{Value, json};
use yrs::{Doc, Map, MapRef, Out, Transact};

use crate::schema::{Prop, check_tree, read_tree};
use crate::snapshot::*;
use crate::workspace::DocMeta;
use crate::*;

struct SeqIds(usize);
impl Ids for SeqIds {
    fn next_id(&mut self) -> String {
        self.0 += 1;
        format!("b{}", self.0)
    }
}

fn meta() -> DocMeta {
    let mut m = DocMeta::new("docA", "", 1_700_000_000_000.0);
    m.tags = vec!["t1".into()];
    m
}

fn text(delta: Value) -> Value {
    json!({ TEXT_MARK: true, "delta": delta })
}

/// 手写的一份快照：段落带格式和行内引用、表格（拍平存）、白板上一个带文字的元素、Boxed 的值、嵌套对象和数组。
fn sample() -> Value {
    json!({
        "type": "page",
        "meta": { "id": "docA", "title": "示例", "createDate": 1, "tags": ["t1"] },
        "blocks": {
            "type": "block", "id": "p", "flavour": "affine:page", "version": 2,
            "props": { "title": text(json!([{ "insert": "示例" }])) },
            "children": [
                {
                    "type": "block", "id": "s", "flavour": "affine:surface", "version": 5,
                    "props": { "elements": {
                        "e1": {
                            "type": "shape", "id": "e1", "index": "a0",
                            "xywh": "[0,0,10,10]",
                            "text": { "affine:surface:text": true, "delta": [{ "insert": "框里的字" }] },
                            "children": { "affine:surface:ymap": true, "json": { "x": true } }
                        }
                    } },
                    "children": []
                },
                {
                    "type": "block", "id": "n", "flavour": "affine:note", "version": 1,
                    "props": {
                        "displayMode": "both",
                        "background": { "dark": "#000000", "light": "#ffffff" },
                        "edgeless": { "style": { "borderRadius": 8 } }
                    },
                    "children": [
                        {
                            "type": "block", "id": "para", "flavour": "affine:paragraph", "version": 1,
                            "props": {
                                "type": "text",
                                "text": text(json!([
                                    { "insert": "粗体", "attributes": { "bold": true } },
                                    { "insert": "和高亮", "attributes": { "background": "var(--affine-text-highlight-orange)" } },
                                    { "insert": " ", "attributes": { "reference": { "type": "LinkedPage", "pageId": "docB" } } }
                                ])),
                                "meta:createdAt": 1744858915562u64
                            },
                            "children": []
                        },
                        {
                            "type": "block", "id": "tbl", "flavour": "affine:table", "version": 1,
                            "props": {
                                "rows": { "r1": { "rowId": "r1", "order": "a0" } },
                                "columns": { "c1": { "columnId": "c1", "order": "a0", "width": 180 } },
                                "cells": { "r1:c1": { "text": text(json!([{ "insert": "格子" }])) } }
                            },
                            "children": []
                        },
                        {
                            "type": "block", "id": "db", "flavour": "affine:database", "version": 3,
                            "props": {
                                "title": text(json!([{ "insert": "任务" }])),
                                "views": [{ "id": "v1", "name": "表格", "columns": [{ "id": "c1", "hide": false }] }],
                                "columns": [],
                                "cells": {},
                                "extra": { NATIVE_MARK: true, "value": { "raw": [1, 2] } }
                            },
                            "children": []
                        }
                    ]
                }
            ]
        }
    })
}

#[test]
fn snapshot_round_trips_through_yjs() {
    let doc = Doc::new();
    let m = from_snapshot(&doc, &sample()).unwrap();
    assert_eq!(m.id, "docA");
    assert_eq!(m.title, "示例");
    assert_eq!(m.tags, ["t1"]);
    assert_eq!(check_tree(&read_tree(&doc).unwrap()), []);
    let back = to_snapshot(&doc, &m).unwrap();
    assert_eq!(back, sample());
}

#[test]
fn snapshot_is_stored_the_blocksuite_way() {
    let doc = Doc::new();
    from_snapshot(&doc, &sample()).unwrap();
    let blocks = doc.get_or_insert_map("blocks");
    let txn = doc.transact();
    let get = |id: &str| -> MapRef {
        match blocks.get(&txn, id) {
            Some(Out::YMap(m)) => m,
            _ => panic!("{id}"),
        }
    };
    // 表格拍平存，格子里是 Y.Text。
    let tbl = get("tbl");
    assert!(matches!(
        tbl.get(&txn, "prop:rows.r1.order"),
        Some(Out::Any(_))
    ));
    assert!(matches!(
        tbl.get(&txn, "prop:cells.r1:c1.text"),
        Some(Out::YText(_))
    ));
    assert!(tbl.get(&txn, "prop:rows").is_none());
    // 嵌套对象是 Y.Map，数组是 Y.Array。
    let note = get("n");
    assert!(matches!(
        note.get(&txn, "prop:background"),
        Some(Out::YMap(_))
    ));
    let db = get("db");
    assert!(matches!(db.get(&txn, "prop:views"), Some(Out::YArray(_))));
    // Boxed：Y.Map {type, value}，value 原样存。
    let Some(Out::YMap(boxed)) = db.get(&txn, "prop:extra") else {
        panic!()
    };
    assert!(matches!(boxed.get(&txn, "type"), Some(Out::Any(_))));
    assert!(matches!(boxed.get(&txn, "value"), Some(Out::Any(_))));
    // 白板元素：Boxed 里是元素的 Y.Map，文字是 Y.Text。
    let Some(Out::YMap(el)) = get("s").get(&txn, "prop:elements") else {
        panic!()
    };
    let Some(Out::YMap(all)) = el.get(&txn, "value") else {
        panic!()
    };
    let Some(Out::YMap(e1)) = all.get(&txn, "e1") else {
        panic!()
    };
    assert!(matches!(e1.get(&txn, "text"), Some(Out::YText(_))));
    assert!(matches!(e1.get(&txn, "children"), Some(Out::YMap(_))));
}

#[test]
fn documents_we_write_become_snapshots() {
    let doc = Doc::new();
    create(
        &doc,
        "流程",
        &to_blocks("## 一条\n\n正文 **粗体**\n\n- 列表\n"),
        &mut SeqIds(0),
    )
    .unwrap();
    let snap = to_snapshot(&doc, &meta()).unwrap();
    assert_eq!(snap["meta"]["title"], "流程");
    assert_eq!(snap["meta"]["tags"], json!(["t1"]));
    assert_eq!(snap["blocks"]["flavour"], "affine:page");
    // 快照写回另一篇文档，读出来的 Markdown 一样。
    let other = Doc::new();
    from_snapshot(&other, &snap).unwrap();
    assert_eq!(
        to_markdown(&read(&other).unwrap().blocks),
        to_markdown(&read(&doc).unwrap().blocks)
    );
    assert_eq!(
        read_tree(&other)
            .unwrap()
            .prop("title")
            .and_then(Prop::plain_text)
            .as_deref(),
        Some("流程")
    );
}

#[test]
fn bad_snapshots_leave_the_document_alone() {
    let mut dup = sample();
    dup["blocks"]["children"][1]["children"][1]["id"] = json!("para");
    let doc = Doc::new();
    assert_eq!(
        from_snapshot(&doc, &dup),
        Err(SnapshotError::DuplicateId("para".into()))
    );
    // 出错时什么也没写。
    let blocks = doc.get_or_insert_map("blocks");
    assert_eq!(blocks.len(&doc.transact()), 0);

    assert!(matches!(
        from_snapshot(&doc, &json!({ "type": "slice" })),
        Err(SnapshotError::Format(_))
    ));
    from_snapshot(&doc, &sample()).unwrap();
    assert_eq!(from_snapshot(&doc, &sample()), Err(SnapshotError::NotEmpty));
}

/// 和 AFFiNE 导出的快照对照，平时不跑：`AFFINE_DOC_SNAPSHOT=<.snapshot.json 文件>`。
/// 读进来、检查结构、再写成快照，和原来的逐项比较。
#[test]
#[ignore]
fn interop_with_affine_snapshot() {
    let Ok(path) = std::env::var("AFFINE_DOC_SNAPSHOT") else {
        return;
    };
    let original: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let doc = Doc::new();
    let m = from_snapshot(&doc, &original).unwrap();
    let problems = check_tree(&read_tree(&doc).unwrap());
    println!("---- 结构检查：{problems:?}");
    let back = to_snapshot(&doc, &m).unwrap();
    println!("---- 和原来一样：{}", back == original);
    if back != original {
        diff("", &original, &back);
    }
}

pub(crate) fn diff(path: &str, a: &Value, b: &Value) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys().chain(y.keys().filter(|k| !x.contains_key(*k))) {
                diff(
                    &format!("{path}/{k}"),
                    x.get(k).unwrap_or(&Value::Null),
                    y.get(k).unwrap_or(&Value::Null),
                );
            }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                diff(&format!("{path}/{i}"), p, q);
            }
        }
        _ if a != b => println!("---- 不一样 {path}: {a} → {b}"),
        _ => {}
    }
}
