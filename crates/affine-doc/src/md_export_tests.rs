use std::collections::HashMap;

use serde_json::{Value, json};
use yrs::Doc;

use crate::md_export::*;
use crate::snapshot::{TEXT_MARK, to_snapshot};
use crate::workspace::DocMeta;
use crate::*;

fn t(delta: Value) -> Value {
    json!({ TEXT_MARK: true, "delta": delta })
}

fn s(text: &str) -> Value {
    t(json!([{ "insert": text }]))
}

fn block(id: &str, flavour: &str, props: Value, children: Vec<Value>) -> Value {
    json!({ "type": "block", "id": id, "flavour": flavour, "version": 1, "props": props, "children": children })
}

fn para(id: &str, kind: &str, text: Value) -> Value {
    block(
        id,
        "affine:paragraph",
        json!({ "type": kind, "text": text }),
        vec![],
    )
}

fn list(id: &str, kind: &str, text: &str, children: Vec<Value>) -> Value {
    block(
        id,
        "affine:list",
        json!({ "type": kind, "text": s(text), "checked": kind == "todo" && id.ends_with('x') }),
        children,
    )
}

fn page(children: Vec<Value>) -> Value {
    json!({
        "type": "page",
        "meta": { "id": "docA", "title": "示例", "createDate": 1, "tags": [] },
        "blocks": block("p", "affine:page", json!({ "title": s("示例") }), vec![
            block("s", "affine:surface", json!({ "elements": { "e1": { "type": "shape" } } }), vec![]),
            block("n", "affine:note", json!({ "displayMode": "both" }), children),
            block("n2", "affine:note", json!({ "displayMode": "edgeless" }), vec![
                para("hidden", "text", s("只在白板上")),
            ]),
        ]),
    })
}

fn cx() -> MdContext {
    MdContext {
        titles: HashMap::from([("docB".to_owned(), "另一篇".to_owned())]),
        doc_link_base: Some("https://affine.example/workspace/ws1".to_owned()),
        assets: HashMap::from([("blobPng".to_owned(), "blobPng.png".to_owned())]),
    }
}

#[test]
fn text_lists_and_code() {
    let snap = page(vec![
        para("h", "h2", s("一条")),
        para(
            "a",
            "text",
            t(json!([
                { "insert": "粗体", "attributes": { "bold": true } },
                { "insert": "、" },
                { "insert": "链接", "attributes": { "link": "https://example.com" } },
                { "insert": "、见 " },
                { "insert": " ", "attributes": { "reference": { "type": "LinkedPage", "pageId": "docB" } } },
                { "insert": "、公式 " },
                { "insert": " ", "attributes": { "latex": "a_1^2" } },
                { "insert": "、红字", "attributes": { "color": "red" } },
            ])),
        ),
        para("q", "quote", s("引用")),
        list(
            "l1",
            "bulleted",
            "甲",
            vec![list("l11", "numbered", "甲一", vec![])],
        ),
        list("l2", "todo", "乙", vec![]),
        list("l3x", "todo", "丙", vec![]),
        block(
            "c",
            "affine:code",
            json!({ "language": "rust", "text": s("let a = 1; // `x`") }),
            vec![],
        ),
        block("d", "affine:divider", json!({}), vec![]),
        block("m", "affine:latex", json!({ "latex": "E = mc^2" }), vec![]),
    ]);
    let out = snapshot_to_markdown(&snap, &cx());
    assert_eq!(
        out.markdown,
        "# 示例\n\n\
         ## 一条\n\n\
         **粗体**、[链接](https://example.com)、见 [另一篇](https://affine.example/workspace/ws1/docB)、公式 $a_1^2$、红字\n\n\
         > 引用\n\n\
         - 甲\n  1. 甲一\n\
         - [ ] 乙\n\
         - [x] 丙\n\n\
         ```rust\nlet a = 1; // `x`\n```\n\n\
         ---\n\n\
         $$\nE = mc^2\n$$"
    );
    assert_eq!(
        out.lossy,
        ["白板", "文字格式 color", "只在白板里显示的 note"]
    );
}

#[test]
fn images_attachments_and_links() {
    let snap = page(vec![
        block(
            "i1",
            "affine:image",
            json!({ "sourceId": "blobPng", "caption": "图[1]" }),
            vec![],
        ),
        block("i2", "affine:image", json!({ "sourceId": "gone" }), vec![]),
        block(
            "f",
            "affine:attachment",
            json!({ "sourceId": "blobPng", "name": "a.pdf" }),
            vec![],
        ),
        block(
            "b",
            "affine:bookmark",
            json!({ "url": "https://example.com/a b", "title": "书签" }),
            vec![],
        ),
        block(
            "e",
            "affine:embed-linked-doc",
            json!({ "pageId": "docB" }),
            vec![],
        ),
        block(
            "e2",
            "affine:embed-synced-doc",
            json!({ "pageId": "noTitle" }),
            vec![],
        ),
        block(
            "co",
            "affine:callout",
            json!({ "emoji": "💡" }),
            vec![para("cp", "text", s("提示"))],
        ),
        block("fr", "affine:frame", json!({}), vec![]),
    ]);
    let out = snapshot_to_markdown(&snap, &cx());
    assert_eq!(
        out.markdown,
        "# 示例\n\n\
         ![图\\[1\\]](assets/blobPng.png)\n\n\
         [a.pdf](assets/blobPng.png)\n\n\
         [书签](<https://example.com/a b>)\n\n\
         [另一篇](https://affine.example/workspace/ws1/docB)\n\n\
         [untitled](https://affine.example/workspace/ws1/noTitle)\n\n\
         > 💡\n>\n> 提示"
    );
    assert_eq!(out.used_assets, ["blobPng"]);
    assert_eq!(out.missing_assets, ["gone"]);
    assert!(out.lossy.contains(&"块 affine:frame".to_owned()));

    // 没有链接前缀时，引用只写标题。
    let plain = MdContext {
        doc_link_base: None,
        ..cx()
    };
    let out = snapshot_to_markdown(&snap, &plain);
    assert!(
        out.markdown.contains("\n\n另一篇\n\nuntitled\n\n"),
        "{}",
        out.markdown
    );
}

#[test]
fn tables_and_databases() {
    let table = block(
        "tbl",
        "affine:table",
        json!({
            // 行、列按 order 排，不按键的先后。
            "rows": { "r2": { "rowId": "r2", "order": "a1" }, "r1": { "rowId": "r1", "order": "a0" } },
            "columns": { "c2": { "columnId": "c2", "order": "a1" }, "c1": { "columnId": "c1", "order": "a0" } },
            "cells": {
                "r1:c1": { "text": s("名字") }, "r1:c2": { "text": s("说明") },
                "r2:c1": { "text": s("甲|乙") }, "r2:c2": { "text": s("两行\n第二行") },
            },
        }),
        vec![],
    );
    let db = block(
        "db",
        "affine:database",
        json!({
            "title": s("任务"),
            "views": [],
            "columns": [
                { "id": "t", "type": "title", "name": "标题", "data": {} },
                { "id": "st", "type": "select", "name": "状态", "data": { "options": [
                    { "id": "o1", "value": "进行中", "color": "x" },
                    { "id": "o2", "value": "完成", "color": "y" },
                ] } },
                { "id": "tags", "type": "multi-select", "name": "标签", "data": { "options": [
                    { "id": "o1", "value": "甲", "color": "x" },
                    { "id": "o2", "value": "乙", "color": "y" },
                ] } },
                { "id": "ok", "type": "checkbox", "name": "好了", "data": {} },
                { "id": "due", "type": "date", "name": "截止", "data": {} },
                { "id": "n", "type": "number", "name": "分", "data": {} },
                { "id": "note", "type": "rich-text", "name": "备注", "data": {} },
            ],
            "cells": {
                "row1": {
                    "st": { "columnId": "st", "value": "o2" },
                    "tags": { "columnId": "tags", "value": ["o2", "o1"] },
                    "ok": { "columnId": "ok", "value": true },
                    "due": { "columnId": "due", "value": 1_764_028_800_000u64 },
                    "n": { "columnId": "n", "value": 3.5 },
                    "note": { "columnId": "note", "value": s("**不是粗体**") },
                },
            },
        }),
        vec![
            para("row1", "text", s("写方案")),
            para("row2", "text", s("空的一行")),
        ],
    );
    let out = snapshot_to_markdown(&page(vec![table, db]), &cx());
    assert_eq!(
        out.markdown,
        "# 示例\n\n\
         | 名字 | 说明 |\n| --- | --- |\n| 甲\\|乙 | 两行<br>第二行 |\n\n\
         任务\n\n\
         | 标题 | 状态 | 标签 | 好了 | 截止 | 分 | 备注 |\n\
         | --- | --- | --- | --- | --- | --- | --- |\n\
         | 写方案 | 完成 | 乙, 甲 | ✔ | 2025-11-25 | 3.5 | \\*\\*不是粗体\\*\\* |\n\
         | 空的一行 |  |  |  |  |  |  |"
    );
    // 表格读回来是 GFM 表格，列数对。
    let back = to_blocks(&out.markdown);
    assert!(
        back.iter().any(|b| b.lossy),
        "表格在同步用的子集里还是有损失的"
    );
}

#[test]
fn documents_we_write_export_like_the_sync_markdown() {
    // 同步用的子集写出来的文档，整篇导出和子集的 Markdown 一样（多一个标题）。
    let doc = Doc::new();
    let md = "## 一条\n\n正文 **粗体** 和 `代码`\n\n- 列表\n  - 下一级\n\n1. 编号\n\n> 引用\n\n```js\nx\n```\n\n---";
    create(&doc, "流程", &to_blocks(md), &mut RandomIds).unwrap();
    let snap = to_snapshot(&doc, &DocMeta::new("d", "流程", 1.0)).unwrap();
    let out = snapshot_to_markdown(&snap, &MdContext::default());
    assert_eq!(
        out.markdown,
        format!("# 流程\n\n{}", to_markdown(&read(&doc).unwrap().blocks))
    );
    assert!(out.lossy.is_empty(), "{:?}", out.lossy);
}

/// 拿 AFFiNE 导出的包导出成 Markdown 看一眼，平时不跑：`AFFINE_DOC_BUNDLE=<.bs.zip 文件>`。
/// 每篇打印出来，再检查 comrak 读得进去、图片都找得到附件。
#[test]
#[ignore]
fn export_affine_bundle_to_markdown() {
    let Ok(path) = std::env::var("AFFINE_DOC_BUNDLE") else {
        return;
    };
    let bundle = crate::bundle::read_bundle(&std::fs::read(path).unwrap()).unwrap();
    let mut cx = MdContext::default();
    for snap in &bundle.docs {
        let id = snap
            .pointer("/meta/id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let title = snap
            .pointer("/meta/title")
            .and_then(Value::as_str)
            .unwrap_or_default();
        cx.titles.insert(id.to_owned(), title.to_owned());
    }
    for (id, a) in &bundle.assets {
        cx.assets.insert(id.clone(), format!("{id}.{}", a.ext));
    }
    for snap in &bundle.docs {
        let out = snapshot_to_markdown(snap, &cx);
        println!("{}", out.markdown);
        println!(
            "---- {} 字，用了 {} 个附件，缺 {:?}，丢掉的 {:?}",
            out.markdown.chars().count(),
            out.used_assets.len(),
            out.missing_assets,
            out.lossy
        );
    }
}
