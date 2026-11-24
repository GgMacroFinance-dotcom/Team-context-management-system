use std::collections::BTreeMap;
use std::io::{Cursor, Write};

use serde_json::{Value, json};
use yrs::Doc;

use crate::bundle::*;
use crate::schema::{check_tree, read_tree};
use crate::snapshot::{TEXT_MARK, from_snapshot, to_snapshot};
use crate::workspace::{self, DocMeta};
use crate::*;

struct SeqIds(usize);
impl Ids for SeqIds {
    fn next_id(&mut self) -> String {
        self.0 += 1;
        format!("new{}", self.0)
    }
}

/// 空文字的 delta 是空数组，和 AFFiNE 导出的一样。
fn text(s: &str) -> Value {
    let delta = if s.is_empty() {
        json!([])
    } else {
        json!([{ "insert": s }])
    };
    json!({ TEXT_MARK: true, "delta": delta })
}

/// 一篇文档：标题、一段带行内引用的文字、一张图、一个附件，可选一个嵌入另一篇文档的卡片。
fn page(id: &str, title: &str, link_to: &str, blob: &str) -> Value {
    json!({
        "type": "page",
        "meta": { "id": id, "title": title, "createDate": 1, "tags": [] },
        "blocks": {
            "type": "block", "id": "p", "flavour": "affine:page", "version": 2,
            "props": { "title": text(title) },
            "children": [{
                "type": "block", "id": "n", "flavour": "affine:note", "version": 1,
                "props": { "displayMode": "both" },
                "children": [
                    {
                        "type": "block", "id": "para", "flavour": "affine:paragraph", "version": 1,
                        "props": { "type": "text", "text": { TEXT_MARK: true, "delta": [
                            { "insert": "见 " },
                            { "insert": " ", "attributes": { "reference": { "type": "LinkedPage", "pageId": link_to } } }
                        ] } },
                        "children": []
                    },
                    {
                        "type": "block", "id": "img", "flavour": "affine:image", "version": 1,
                        "props": { "sourceId": blob, "caption": "图", "width": 0, "height": 0 },
                        "children": []
                    },
                    {
                        "type": "block", "id": "att", "flavour": "affine:attachment", "version": 1,
                        "props": { "name": "a.pdf", "size": 3, "type": "application/pdf", "sourceId": "blobPdf" },
                        "children": []
                    },
                    {
                        "type": "block", "id": "card", "flavour": "affine:embed-linked-doc", "version": 1,
                        "props": { "pageId": link_to, "style": "horizontal" },
                        "children": []
                    }
                ]
            }]
        }
    })
}

fn load(snap: &Value) -> (Doc, DocMeta) {
    let doc = Doc::new();
    let meta = from_snapshot(&doc, snap).unwrap();
    (doc, meta)
}

fn asset(ext: &str, bytes: &[u8]) -> Asset {
    Asset {
        ext: ext.into(),
        bytes: bytes.to_vec(),
    }
}

fn files(zip: &[u8]) -> Vec<String> {
    let mut z = zip::ZipArchive::new(Cursor::new(zip)).unwrap();
    (0..z.len())
        .map(|i| z.by_index(i).unwrap().name().unwrap().into_owned())
        .collect()
}

#[test]
fn export_then_read_back() {
    let a = page("docA", "甲/一", "docB", "blobPng");
    let b = page("docB", "", "docA", "blobPng");
    let (da, ma) = load(&a);
    let (db, mb) = load(&b);
    let mut asked = Vec::new();
    let out = export_docs(&[(&da, &ma), (&db, &mb)], |id| {
        asked.push(id.to_owned());
        match id {
            "blobPng" => Some(asset(ext_for_mime("image/png"), b"png")),
            _ => None,
        }
    })
    .unwrap();
    // 同一个附件只取一次；取不到的记下来，照样导出。
    assert_eq!(asked, ["blobPng", "blobPdf"]);
    assert_eq!(out.missing_assets, ["blobPdf"]);
    assert_eq!(
        files(&out.zip),
        [
            "甲_一-docA.snapshot.json",
            "untitled-docB.snapshot.json",
            "assets/blobPng.png"
        ]
    );
    let back = read_bundle(&out.zip).unwrap();
    assert_eq!(back.docs, [a, b]);
    assert_eq!(
        back.assets,
        BTreeMap::from([("blobPng".to_owned(), asset("png", b"png"))])
    );
    assert_eq!(asset_ids(&back.docs[0]), ["blobPng", "blobPdf"]);
}

#[test]
fn import_gives_new_ids_and_keeps_links() {
    let bundle = Bundle {
        docs: vec![
            page("docA", "甲", "docB", "blobPng"),
            page("docB", "乙", "elsewhere", "blobPng"),
        ],
        assets: BTreeMap::new(),
    };
    let root = Doc::new();
    let got = import_docs(&root, &bundle, &mut SeqIds(0)).unwrap();
    let ids: Vec<_> = got.iter().map(|d| d.meta.id.as_str()).collect();
    assert_eq!(ids, ["new1", "new2"]);
    // 引用包里另一篇的换成新 id，引用包外的不动。
    let a = to_snapshot(&got[0].doc, &got[0].meta).unwrap();
    let note = &a["blocks"]["children"][0]["children"];
    assert_eq!(
        note[0]["props"]["text"]["delta"][1]["attributes"]["reference"]["pageId"],
        "new2"
    );
    assert_eq!(note[3]["props"]["pageId"], "new2");
    let b = to_snapshot(&got[1].doc, &got[1].meta).unwrap();
    assert_eq!(
        b["blocks"]["children"][0]["children"][3]["props"]["pageId"],
        "elsewhere"
    );
    // 除了 id，和原来一样；结构检查没问题；都记进了文档列表。
    let mut want = bundle.docs[0].clone();
    want["meta"]["id"] = json!("new1");
    want["blocks"]["children"][0]["children"][0]["props"]["text"]["delta"][1]["attributes"]["reference"]
        ["pageId"] = json!("new2");
    want["blocks"]["children"][0]["children"][3]["props"]["pageId"] = json!("new2");
    assert_eq!(a, want);
    assert_eq!(check_tree(&read_tree(&got[0].doc).unwrap()), []);
    let pages = workspace::read_meta(&root).pages;
    let listed: Vec<_> = pages
        .iter()
        .map(|p| (p.id.as_str(), p.title.as_str()))
        .collect();
    assert_eq!(listed, [("new1", "甲"), ("new2", "乙")]);
}

#[test]
fn documents_we_write_survive_a_bundle() {
    let doc = Doc::new();
    create(
        &doc,
        "流程",
        &to_blocks("## 一条\n\n正文 **粗体**\n"),
        &mut RandomIds,
    )
    .unwrap();
    let meta = DocMeta::new("mine", "流程", 1.7e12);
    let out = export_docs(&[(&doc, &meta)], |_| None).unwrap();
    let root = Doc::new();
    let got = import_docs(&root, &read_bundle(&out.zip).unwrap(), &mut SeqIds(0)).unwrap();
    assert_eq!(
        to_markdown(&read(&got[0].doc).unwrap().blocks),
        to_markdown(&read(&doc).unwrap().blocks)
    );
}

fn raw_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    for (name, bytes) in entries {
        if name.ends_with('/') {
            z.add_directory(*name, opts).unwrap();
        } else {
            z.start_file(*name, opts).unwrap();
            z.write_all(bytes).unwrap();
        }
    }
    z.finish().unwrap().into_inner()
}

#[test]
fn reading_skips_junk_and_rejects_bad_files() {
    let snap = serde_json::to_vec(&page("docA", "甲", "x", "b1")).unwrap();
    let zip = raw_zip(&[
        ("__MACOSX/._甲-docA.snapshot.json", b"junk"),
        (".DS_Store", b"junk"),
        ("info.json", b"{}"),
        ("notes.txt", b"junk"),
        ("assets/", b""),
        ("assets/b1.png", b"png"),
        ("assets/noext", b"x"),
        ("外层/assets/b2.svg", b"svg"),
        ("甲-docA.snapshot.json", &snap),
    ]);
    let got = read_bundle(&zip).unwrap();
    assert_eq!(got.docs.len(), 1);
    assert_eq!(got.assets.keys().collect::<Vec<_>>(), ["b1", "b2", "noext"]);
    assert_eq!(got.assets["noext"].ext, "");

    let not_json = raw_zip(&[("a-x.snapshot.json", b"{")]);
    assert!(matches!(read_bundle(&not_json), Err(BundleError::Json(..))));
    let not_page = raw_zip(&[("a-x.snapshot.json", br#"{"type":"slice"}"#)]);
    assert!(matches!(
        read_bundle(&not_page),
        Err(BundleError::Snapshot(..))
    ));
    assert!(matches!(
        read_bundle(b"not a zip"),
        Err(BundleError::Zip(_))
    ));
}

#[test]
fn a_bad_doc_imports_nothing() {
    let mut broken = page("docB", "乙", "x", "b");
    broken["blocks"]["children"][0]["children"][1]["id"] = json!("para");
    let bundle = Bundle {
        docs: vec![page("docA", "甲", "x", "b"), broken],
        assets: BTreeMap::new(),
    };
    let root = Doc::new();
    assert!(matches!(
        import_docs(&root, &bundle, &mut SeqIds(0)),
        Err(BundleError::Snapshot(..))
    ));
    assert!(workspace::read_meta(&root).pages.is_empty());

    let dup = Bundle {
        docs: vec![page("docA", "甲", "x", "b"), page("docA", "乙", "x", "b")],
        assets: BTreeMap::new(),
    };
    assert!(matches!(
        import_docs(&root, &dup, &mut SeqIds(0)),
        Err(BundleError::DuplicateDoc(id)) if id == "docA"
    ));
    assert!(matches!(
        write_bundle(&dup),
        Err(BundleError::DuplicateDoc(_))
    ));
}

#[test]
fn mime_to_ext() {
    assert_eq!(ext_for_mime("image/png"), "png");
    assert_eq!(ext_for_mime("image/jpeg"), "jpeg");
    assert_eq!(ext_for_mime("image/svg+xml"), "svg");
    assert_eq!(ext_for_mime(""), "blob");
    assert_eq!(ext_for_mime("application/x-whatever"), "x-whatever");
}

/// 和 AFFiNE 导出的包对照，平时不跑：`AFFINE_DOC_BUNDLE=<.bs.zip 文件>`。
/// 解开、每篇写进 Yjs 再写回快照逐项比较，再整包导入、导出一遍看文件是不是一样多。
#[test]
#[ignore]
fn interop_with_affine_bundle() {
    let Ok(path) = std::env::var("AFFINE_DOC_BUNDLE") else {
        return;
    };
    let bundle = read_bundle(&std::fs::read(path).unwrap()).unwrap();
    println!(
        "---- {} 篇文档，{} 个附件",
        bundle.docs.len(),
        bundle.assets.len()
    );
    for snap in &bundle.docs {
        let (doc, meta) = load(snap);
        let problems = check_tree(&read_tree(&doc).unwrap());
        let back = to_snapshot(&doc, &meta).unwrap();
        println!(
            "---- {}：结构检查 {problems:?}，转回来一样：{}",
            meta.title,
            back == *snap
        );
        if back != *snap {
            crate::snapshot_tests::diff("", snap, &back);
        }
    }
    let root = Doc::new();
    let got = import_docs(&root, &bundle, &mut RandomIds).unwrap();
    let docs: Vec<_> = got.iter().map(|d| (&d.doc, &d.meta)).collect();
    let out = export_docs(&docs, |id| bundle.assets.get(id).cloned()).unwrap();
    let again = read_bundle(&out.zip).unwrap();
    println!(
        "---- 再导出：{} 篇，{} 个附件，缺的附件 {:?}",
        again.docs.len(),
        again.assets.len(),
        out.missing_assets
    );
    if let Ok(dir) = std::env::var("AFFINE_DOC_BUNDLE_OUT") {
        std::fs::write(format!("{dir}/reexported.bs.zip"), &out.zip).unwrap();
    }
}
