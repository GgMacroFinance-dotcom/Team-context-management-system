use std::collections::BTreeMap;

use yrs::updates::decoder::Decode;
use yrs::{
    Any, Array, ArrayPrelim, ArrayRef, Doc, Map, MapPrelim, MapRef, Out, ReadTxn, StateVector,
    Text, TextPrelim, TextRef, Transact, Update,
};

use crate::schema::*;
use crate::*;

struct SeqIds(usize);
impl Ids for SeqIds {
    fn next_id(&mut self) -> String {
        self.0 += 1;
        format!("b{}", self.0)
    }
}

fn new_doc(md: &str) -> Doc {
    let doc = Doc::new();
    create(&doc, "标题", &to_blocks(md), &mut SeqIds(0)).unwrap();
    doc
}

/// 第一个 note 的 id 和它的子块数组。
fn first_note(doc: &Doc) -> (String, ArrayRef) {
    let map = doc.get_or_insert_map("blocks");
    let txn = doc.transact();
    map.iter(&txn)
        .find_map(|(id, v)| match v {
            Out::YMap(b) => match (b.get(&txn, "sys:flavour"), b.get(&txn, "sys:children")) {
                (Some(Out::Any(Any::String(f))), Some(Out::YArray(a))) if &*f == "affine:note" => {
                    Some((id.to_owned(), a))
                }
                _ => None,
            },
            _ => None,
        })
        .unwrap()
}

/// 在 `parent` 的子块最后加一个块，`props` 由调用的人往里写。
fn add_block(
    doc: &Doc,
    parent: &ArrayRef,
    id: &str,
    flavour: &str,
    version: i64,
    props: impl FnOnce(&mut yrs::TransactionMut, &MapRef),
) {
    let map = doc.get_or_insert_map("blocks");
    let mut txn = doc.transact_mut();
    let m: MapRef = map.insert(&mut txn, id, MapPrelim::default());
    m.insert(&mut txn, "sys:id", id);
    m.insert(&mut txn, "sys:flavour", flavour);
    m.insert(&mut txn, "sys:version", Any::from(version));
    m.insert(&mut txn, "sys:children", ArrayPrelim::default());
    props(&mut txn, &m);
    let len = parent.len(&txn);
    parent.insert(&mut txn, len, id);
}

#[test]
fn catalog_has_every_block_once() {
    assert_eq!(FLAVOURS.len(), 28);
    let mut names: Vec<&str> = FLAVOURS.iter().map(|f| f.name).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), FLAVOURS.len());
    assert_eq!(flavour("affine:page").unwrap().version, 2);
    assert_eq!(flavour("affine:surface").unwrap().version, 5);
    assert_eq!(flavour("affine:database").unwrap().version, 3);
    assert!(flavour("affine:table").unwrap().flat_data);
    assert!(flavour("affine:nope").is_none());
}

#[test]
fn documents_we_write_pass_the_check() {
    let doc =
        new_doc("## 一条\n\n正文 **粗体**\n\n- 列表\n  - 子项\n\n```rust\nfn x() {}\n```\n\n---\n");
    let tree = read_tree(&doc).unwrap();
    assert_eq!(tree.flavour, "affine:page");
    assert_eq!(
        tree.prop("title").and_then(Prop::plain_text).as_deref(),
        Some("标题")
    );
    let kinds: Vec<&str> = tree.walk().iter().map(|b| b.flavour.as_str()).collect();
    assert!(kinds.contains(&"affine:surface"));
    assert!(kinds.contains(&"affine:code"));
    assert_eq!(check_tree(&tree), []);
}

#[test]
fn blocks_markdown_cannot_hold_are_read_in_full() {
    let doc = new_doc("## 一条\n\n正文\n");
    let (_, note) = first_note(&doc);
    add_block(&doc, &note, "img", "affine:image", 1, |txn, m| {
        m.insert(txn, "prop:sourceId", "blob-1");
        m.insert(txn, "prop:width", 640.0);
    });
    add_block(
        &doc,
        &note,
        "link",
        "affine:embed-linked-doc",
        1,
        |txn, m| {
            m.insert(txn, "prop:pageId", "docB");
        },
    );
    // 表格：属性拍平存。
    add_block(&doc, &note, "tbl", "affine:table", 1, |txn, m| {
        m.insert(txn, "prop:rows.r1.rowId", "r1");
        m.insert(txn, "prop:rows.r1.order", "a0");
        m.insert(txn, "prop:columns.c1.columnId", "c1");
        let t: TextRef = m.insert(txn, "prop:cells.r1:c1.text", TextPrelim::new(""));
        t.insert(txn, 0, "格子");
    });
    // 数据库：嵌套的 Y.Map，文字列是 Y.Text；标题带一个不认得的格式。
    add_block(&doc, &note, "db", "affine:database", 3, |txn, m| {
        let title: TextRef = m.insert(txn, "prop:title", TextPrelim::new(""));
        let mut attrs = yrs::types::Attrs::new();
        attrs.insert("color".into(), "red".into());
        title.insert_with_attributes(txn, 0, "任务表", attrs);
        let cells: MapRef = m.insert(txn, "prop:cells", MapPrelim::default());
        let row: MapRef = cells.insert(txn, "row1", MapPrelim::default());
        let cell: MapRef = row.insert(txn, "col1", MapPrelim::default());
        cell.insert(txn, "columnId", "col1");
        let v: TextRef = cell.insert(txn, "value", TextPrelim::new(""));
        v.insert(txn, 0, "值");
    });

    // 编码成更新在别处读，和从 AFFiNE 收到的一样。
    let update = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    let other = Doc::new();
    other
        .transact_mut()
        .apply_update(Update::decode_v1(&update).unwrap())
        .unwrap();
    let tree = read_tree(&other).unwrap();
    let find = |id: &str| {
        tree.walk()
            .into_iter()
            .find(|b| b.id == id)
            .unwrap()
            .clone()
    };

    let img = find("img");
    assert_eq!(img.prop("sourceId").and_then(Prop::as_str), Some("blob-1"));
    assert_eq!(img.prop("width"), Some(&Prop::Value(Any::from(640.0))));
    assert_eq!(
        find("link").prop("pageId").and_then(Prop::as_str),
        Some("docB")
    );

    let Some(Prop::Map(cells)) = find("tbl").props.get("cells").cloned() else {
        panic!("表格的 cells 没有拼回来");
    };
    let Some(Prop::Map(cell)) = cells.get("r1:c1") else {
        panic!("{cells:?}");
    };
    assert_eq!(
        cell.get("text").and_then(Prop::plain_text).as_deref(),
        Some("格子")
    );

    let db = find("db");
    let Some(Prop::Text(title)) = db.prop("title") else {
        panic!()
    };
    assert_eq!(
        title[0],
        Delta::Text {
            text: "任务表".into(),
            attrs: BTreeMap::from([("color".to_owned(), Any::from("red"))]),
        }
    );
    let Some(Prop::Map(rows)) = db.prop("cells") else {
        panic!()
    };
    let Some(Prop::Map(row)) = rows.get("row1") else {
        panic!()
    };
    let Some(Prop::Map(c)) = row.get("col1") else {
        panic!()
    };
    assert_eq!(
        c.get("value").and_then(Prop::plain_text).as_deref(),
        Some("值")
    );

    assert_eq!(check_tree(&tree), []);
}

#[test]
fn check_finds_misplaced_blocks_and_wrong_versions() {
    let doc = new_doc("正文\n");
    let (_, note) = first_note(&doc);
    // 画框只能在白板里；版本号写错；不认得的块；该是文字的标题写成了普通字符串。
    add_block(&doc, &note, "fr", "affine:frame", 1, |_, _| {});
    add_block(&doc, &note, "old", "affine:database", 2, |txn, m| {
        m.insert(txn, "prop:title", "不是 Y.Text");
    });
    add_block(&doc, &note, "new", "affine:hologram", 1, |_, _| {});
    let problems = check_tree(&read_tree(&doc).unwrap());
    assert!(problems.contains(&Problem::Parent {
        id: "fr".into(),
        flavour: "affine:frame".into(),
        parent: "affine:note".into(),
    }));
    assert!(problems.contains(&Problem::Version {
        id: "old".into(),
        flavour: "affine:database".into(),
        found: Some(2),
        expected: 3,
    }));
    assert!(problems.contains(&Problem::NotText {
        id: "old".into(),
        flavour: "affine:database".into(),
        prop: "title".into(),
    }));
    assert!(problems.contains(&Problem::UnknownFlavour {
        id: "new".into(),
        flavour: "affine:hologram".into(),
    }));
    assert_eq!(problems.len(), 4);
}

#[test]
fn a_doc_without_page_has_no_tree() {
    assert_eq!(read_tree(&Doc::new()), None);
}
