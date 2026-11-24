use yrs::updates::decoder::Decode;
use yrs::{Any, Array, ArrayPrelim, Doc, Map, MapPrelim, ReadTxn, StateVector, Transact, Update};

use crate::doc::Ids;
use crate::workspace::*;

struct Seq(u32);
impl Ids for Seq {
    fn next_id(&mut self) -> String {
        self.0 += 1;
        format!("n{}", self.0)
    }
}

/// 编码成更新，在一个新的 Doc 里应用：别的端收到的样子。
fn replicate(doc: &Doc) -> Doc {
    let update = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    let other = Doc::new();
    other
        .transact_mut()
        .apply_update(Update::decode_v1(&update).unwrap())
        .unwrap();
    other
}

#[test]
fn doc_list_is_written_and_read_back() {
    let root = Doc::new();
    set_workspace_name(&root, "团队上下文");
    let mut a = DocMeta::new("docA", "流程", 1_700_000_000_000.0);
    a.tags = vec!["t1".into()];
    upsert_doc_meta(&root, &a);
    upsert_doc_meta(&root, &DocMeta::new("docB", "术语", 1_700_000_000_001.0));

    let got = read_meta(&replicate(&root));
    assert_eq!(got.name.as_deref(), Some("团队上下文"));
    assert_eq!(got.pages.len(), 2);
    assert_eq!(got.pages[0], a);

    // 改一项：还在原来的位置，不多出一项。
    a.title = "流程（新）".into();
    a.trash = true;
    upsert_doc_meta(&root, &a);
    let got = read_meta(&root);
    assert_eq!(got.pages.len(), 2);
    assert_eq!(got.pages[0].title, "流程（新）");
    assert!(got.pages[0].trash);

    assert!(remove_doc_meta(&root, "docA"));
    assert!(!remove_doc_meta(&root, "docA"));
    let got = read_meta(&root);
    assert_eq!(
        got.pages.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        ["docB"]
    );
}

#[test]
fn doc_list_written_the_blocksuite_way_is_read() {
    // BlockSuite 写进去的样子：pages 是 Y.Array，每项是 Y.Map，tags 是 Y.Array；另带一个不认得的字段。
    let root = Doc::new();
    let meta = root.get_or_insert_map("meta");
    {
        let mut txn = root.transact_mut();
        let pages = meta.insert(&mut txn, "pages", ArrayPrelim::default());
        let p = pages.push_back(&mut txn, MapPrelim::default());
        p.insert(&mut txn, "id", "x1");
        p.insert(&mut txn, "title", "周报");
        p.insert(&mut txn, "createDate", 1.0);
        p.insert(&mut txn, "trashDate", 2.0);
        let tags = p.insert(&mut txn, "tags", ArrayPrelim::default());
        tags.push_back(&mut txn, "a");
        // 一项坏数据：没有 id。
        let bad = pages.push_back(&mut txn, MapPrelim::default());
        bad.insert(&mut txn, "title", "没有 id");
    }
    let got = read_meta(&root);
    assert_eq!(got.pages.len(), 1);
    assert_eq!(got.pages[0].tags, ["a"]);
    assert_eq!(got.pages[0].extra.get("trashDate"), Some(&Any::from(2.0)));
}

#[test]
fn table_rows_and_deletion_marker() {
    let t = Doc::new();
    assert_eq!(table_doc_id("ws1", FOLDERS), "db$ws1$folders");
    upsert_row(&t, "r1", &[("id", "r1".into()), ("v", 1.0.into())]);
    upsert_row(&t, "r2", &[("id", "r2".into()), ("v", 2.0.into())]);
    assert_eq!(rows(&t).len(), 2);
    assert!(delete_row(&t, "r1", "id"));
    assert!(!delete_row(&t, "r1", "id"));
    assert_eq!(row(&t, "r1"), None);
    assert_eq!(row(&t, "nope"), None);

    // 别的端看到的：r1 只剩主键和删除标记。
    let other = replicate(&t);
    let m = other.get_or_insert_map("r1");
    let txn = other.transact();
    let mut keys: Vec<String> = m.keys(&txn).map(str::to_owned).collect();
    keys.sort();
    assert_eq!(keys, ["$$DELETED", "id"]);
    drop(txn);
    assert_eq!(
        rows(&other)
            .iter()
            .map(|r| r.key.as_str())
            .collect::<Vec<_>>(),
        ["r2"]
    );

    // 删掉的行重新写就回来了。
    upsert_row(&t, "r1", &[("v", 3.0.into())]);
    assert_eq!(
        row(&t, "r1").unwrap().fields.get("v"),
        Some(&Any::from(3.0))
    );
}

#[test]
fn folder_tree_like_affine() {
    let t = Doc::new();
    let mut ids = Seq(0);
    let research = create_folder(&t, None, "研究组", &mut ids).unwrap();
    let ops = create_folder(&t, None, "运维组", &mut ids).unwrap();
    let sub = create_folder(&t, Some(&research), "宏观", &mut ids).unwrap();
    let link = create_link(&t, &sub, NodeKind::Doc, "docA", &mut ids).unwrap();

    // 根上只能放文件夹，文件夹只能放进文件夹。
    assert_eq!(
        create_link(&t, &link, NodeKind::Doc, "docB", &mut ids),
        Err(FolderError::NotFolder(link.clone()))
    );
    assert_eq!(
        move_node(&t, &link, None),
        Err(FolderError::RootOnlyFolders)
    );
    assert_eq!(
        move_node(&t, &research, Some(&sub)),
        Err(FolderError::Cycle)
    );

    let nodes = folders(&replicate(&t));
    let root: Vec<&str> = children(&nodes, None)
        .iter()
        .map(|n| n.data.as_str())
        .collect();
    assert_eq!(root, ["研究组", "运维组"]);
    let inner = children(&nodes, Some(&sub));
    assert_eq!(inner.len(), 1);
    assert_eq!(inner[0].kind, NodeKind::Doc);
    assert_eq!(inner[0].data, "docA");
    // 排序键是 AFFiNE 的格式：整数部分 + `0` + 32 个随机字符。
    assert_eq!(inner[0].index.len(), 2 + 1 + 32);

    rename_folder(&t, &ops, "运维").unwrap();
    move_node(&t, &research, None).unwrap();
    let nodes = folders(&t);
    let root: Vec<&str> = children(&nodes, None)
        .iter()
        .map(|n| n.data.as_str())
        .collect();
    assert_eq!(root, ["运维", "研究组"]);

    // 删文件夹连同下面的一起删。
    assert_eq!(remove_node(&t, &research).unwrap(), 3);
    let nodes = folders(&t);
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].parent_id, None);
}

#[test]
fn doc_properties_keep_unknown_fields() {
    let t = Doc::new();
    set_doc_properties(
        &t,
        "docA",
        &[("primaryMode", "page".into()), ("custom:abc", "值".into())],
    );
    set_doc_properties(&t, "docA", &[("updatedBy", "u1".into())]);
    let p = doc_properties(&replicate(&t), "docA").unwrap();
    assert_eq!(p.get("primaryMode"), Some(&Any::from("page")));
    assert_eq!(p.get("custom:abc"), Some(&Any::from("值")));
    assert_eq!(p.get("updatedBy"), Some(&Any::from("u1")));
    assert!(!p.contains_key("id"));
    set_doc_properties(&t, "docA", &[("custom:abc", Any::Undefined)]);
    assert!(
        !doc_properties(&t, "docA")
            .unwrap()
            .contains_key("custom:abc")
    );
    assert_eq!(doc_properties(&t, "docB"), None);
}
