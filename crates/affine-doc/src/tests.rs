use yrs::updates::decoder::Decode;
use yrs::{
    Any, Array, ArrayPrelim, ArrayRef, Doc, Map, MapPrelim, MapRef, Out, ReadTxn, StateVector,
    Text, TextPrelim, TextRef, Transact, Update,
};

use crate::*;

/// 测试里按顺序给的 id：`b1`、`b2`……
struct SeqIds(usize);

impl Ids for SeqIds {
    fn next_id(&mut self) -> String {
        self.0 += 1;
        format!("b{}", self.0)
    }
}

fn bold(t: &str) -> Span {
    Span {
        text: t.to_owned(),
        marks: Marks {
            bold: true,
            ..Default::default()
        },
    }
}

const SAMPLE: &str = "段落 **粗体** 和 *斜体*、~~删除~~、`代码`，还有[链接](https://example.com)。
第二行

### 三级标题

> 引用一段

- 一
- 二
  - 二点一
- [ ] 没做
- [x] 做了

1. 第一
2. 第二

```rust
fn main() {}
```

---

<!-- affine block=img123 flavour=affine:image 这一块只在 AFFiNE 里，没有同步 -->";

#[test]
fn markdown_subset_converts_both_ways() {
    let blocks = to_blocks(SAMPLE);
    assert_eq!(blocks.len(), 12, "{blocks:#?}");
    let Content::Paragraph { kind, text, .. } = &blocks[0].content else {
        panic!("第一块应该是段落");
    };
    assert_eq!(*kind, ParagraphKind::Text);
    assert!(text.iter().any(|s| s.text == "粗体" && s.marks.bold));
    assert!(text.iter().any(|s| s.text == "斜体" && s.marks.italic));
    assert!(text.iter().any(|s| s.text == "删除" && s.marks.strike));
    assert!(text.iter().any(|s| s.text == "代码" && s.marks.code));
    assert!(
        text.iter()
            .any(|s| s.text == "链接" && s.marks.link.as_deref() == Some("https://example.com"))
    );
    assert!(plain_text(text).contains("。\n第二行"));
    assert!(matches!(
        blocks[1].content,
        Content::Paragraph {
            kind: ParagraphKind::Heading(3),
            ..
        }
    ));
    assert!(matches!(
        blocks[2].content,
        Content::Paragraph {
            kind: ParagraphKind::Quote,
            ..
        }
    ));
    let Content::List { children, .. } = &blocks[4].content else {
        panic!("第五块应该是列表项");
    };
    assert_eq!(children.len(), 1);
    assert!(matches!(
        blocks[5].content,
        Content::List {
            kind: ListKind::Todo { checked: false },
            ..
        }
    ));
    assert!(matches!(
        blocks[6].content,
        Content::List {
            kind: ListKind::Todo { checked: true },
            ..
        }
    ));
    assert!(matches!(
        blocks[8].content,
        Content::List {
            kind: ListKind::Numbered,
            ..
        }
    ));
    assert_eq!(
        blocks[9].content,
        Content::Code {
            language: Some("rust".into()),
            text: "fn main() {}".into()
        }
    );
    assert_eq!(blocks[10].content, Content::Divider);
    assert_eq!(blocks[11].id.as_deref(), Some("img123"));
    assert!(blocks.iter().all(|b| !b.is_lossy()));

    // 写回去就是原文，再转一遍不变。
    assert_eq!(to_markdown(&blocks), SAMPLE);
    assert!(roundtrips(&blocks));
}

#[test]
fn text_that_looks_like_markdown_is_escaped() {
    let tricky = [
        "# 不是标题",
        "- 不是列表",
        "1. 不是编号",
        "2) 也不是",
        "> 不是引用",
        "a * b * c、snake_case、[方括号]、`反引号`、<b>不是 HTML</b>、&amp;、!感叹号、a|b",
        "    前面四个空格",
        "===",
        "~~~",
        "反斜杠 \\ 一个",
    ];
    for t in tricky {
        let blocks = vec![Block::paragraph(ParagraphKind::Text, vec![Span::plain(t)])];
        let md = to_markdown(&blocks);
        assert_eq!(to_blocks(&md), blocks, "{t:?} 写成了 {md:?}");
    }
    // 格式标记贴着文字，前后的空格放在外面。
    let blocks = vec![Block::paragraph(
        ParagraphKind::Text,
        vec![Span::plain("前"), bold(" 粗体 "), Span::plain("后")],
    )];
    assert_eq!(to_markdown(&blocks), "前 **粗体** 后");
    assert!(roundtrips(&blocks));
    // 行内代码里有反引号。
    let code = Span {
        text: "a`b".into(),
        marks: Marks {
            code: true,
            ..Default::default()
        },
    };
    let blocks = vec![Block::paragraph(ParagraphKind::Text, vec![code])];
    assert!(roundtrips(&blocks), "{}", to_markdown(&blocks));
    // 代码块里有三个反引号。
    let blocks = vec![Block::new(Content::Code {
        language: None,
        text: "```\n里面\n```".into(),
    })];
    assert!(roundtrips(&blocks), "{}", to_markdown(&blocks));
}

#[test]
fn chinese_punctuation_next_to_bold_survives() {
    // 光按 CommonMark 的规矩，标记一边是标点、另一边是汉字时认不出来；开了 CJK 友好的规则就行。
    for inner in ["粗体", "“引号”", "（括号）", "粗体。", "「书名」"] {
        let blocks = vec![Block::paragraph(
            ParagraphKind::Text,
            vec![Span::plain("中文"), bold(inner), Span::plain("中文")],
        )];
        let md = to_markdown(&blocks);
        assert_eq!(to_blocks(&md), blocks, "{md}");
        assert!(roundtrips(&blocks));
    }
}

#[test]
fn unsupported_markdown_keeps_its_text_and_is_marked_lossy() {
    let md = "| a | b |\n| - | - |\n| 1 | 2 |\n\n图 ![说明](https://x/y.png) 和 <span>HTML</span>\n\n<div>块</div>";
    let blocks = to_blocks(md);
    assert_eq!(blocks.len(), 3, "{blocks:#?}");
    assert_eq!(
        blocks[0].content,
        Content::Code {
            language: Some("markdown".into()),
            text: "| a | b |\n| - | - |\n| 1 | 2 |".into()
        }
    );
    assert!(blocks.iter().all(Block::is_lossy));
    let Content::Paragraph { text, .. } = &blocks[1].content else {
        panic!()
    };
    assert!(
        text.iter()
            .any(|s| s.text == "说明" && s.marks.link.as_deref() == Some("https://x/y.png"))
    );
}

#[test]
fn page_is_written_and_read_back() {
    let doc = Doc::new();
    let blocks = to_blocks(SAMPLE);
    let skipped = create(&doc, "流程", &blocks, &mut SeqIds(0)).unwrap();
    // 新文档里没有 img123 这个块。
    assert_eq!(skipped, vec!["img123".to_owned()]);
    let page = read(&doc).unwrap();
    assert_eq!(page.title, "流程");
    assert!(same_content(&page.blocks, &blocks[..11]));
    assert!(page.blocks.iter().all(|b| b.id.is_some()));

    // 编码成 Yjs 更新、在另一个文档里打开，内容一样。
    let update = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    let other = Doc::new();
    other
        .transact_mut()
        .apply_update(Update::decode_v1(&update).unwrap())
        .unwrap();
    assert_eq!(read(&other).unwrap(), page);

    // 已经有内容的文档不能当新文档写。
    assert_eq!(
        create(&doc, "x", &[], &mut SeqIds(100)),
        Err(Error::NotEmpty)
    );
    assert_eq!(read(&Doc::new()), Err(Error::NoPage));
}

/// 一篇有两条的文档：前言、`## 甲`（两段）、`## 乙`（一段）。
fn two_items() -> Doc {
    let doc = Doc::new();
    let md = "前言\n\n## 甲\n\n第一段\n\n第二段\n\n## 乙\n\n乙的正文";
    create(&doc, "文件", &to_blocks(md), &mut SeqIds(0)).unwrap();
    doc
}

fn items(doc: &Doc) -> Vec<Item> {
    split(&read(doc).unwrap().blocks)
        .into_iter()
        .filter_map(|s| match s {
            Section::Item(i) => Some(i),
            Section::Loose(_) => None,
        })
        .collect()
}

fn block_ids(item: &Item) -> Vec<String> {
    item.body.iter().filter_map(|b| b.id.clone()).collect()
}

#[test]
fn page_is_split_into_items_like_the_context_repo() {
    let doc = two_items();
    let sections = split(&read(&doc).unwrap().blocks);
    assert_eq!(sections.len(), 3);
    assert!(matches!(&sections[0], Section::Loose(v) if v.len() == 1));
    let items = items(&doc);
    assert_eq!(items[0].title, "甲");
    assert_eq!(items[0].body_markdown(), "第一段\n\n第二段");
    assert_eq!(items[1].title, "乙");
    assert!(!items[0].lossy);

    // 一级标题结束上一条，它和后面的不算条目。
    let blocks = to_blocks("## 甲\n\n正文\n\n# 一级\n\n散的\n\n## 乙");
    let sections = split(&blocks);
    assert_eq!(sections.len(), 3);
    assert!(matches!(&sections[1], Section::Loose(v) if v.len() == 2));
}

#[test]
fn updating_an_item_only_touches_what_changed() {
    let doc = two_items();
    let before = items(&doc);
    let heading = before[0].heading_id.clone().unwrap();
    let ids_before = block_ids(&before[0]);

    // 只改第二段，标题也改。
    let body = to_blocks("第一段\n\n第二段改了\n\n- 新加的");
    let skipped = update_item(&doc, &heading, "甲（新）", &body, &mut SeqIds(100)).unwrap();
    assert!(skipped.is_empty());
    let after = items(&doc);
    assert_eq!(
        after[0].heading_id.as_deref(),
        Some(heading.as_str()),
        "标题块的 id 不变"
    );
    assert_eq!(after[0].title, "甲（新）");
    assert_eq!(after[0].body_markdown(), "第一段\n\n第二段改了\n\n- 新加的");
    let ids_after = block_ids(&after[0]);
    assert_eq!(ids_after[0], ids_before[0], "没变的段落原样留着");
    assert_ne!(ids_after[1], ids_before[1]);
    // 别的条目不动。
    assert_eq!(after[1], before[1]);
    // 删掉的块真的从表里删了。
    let map = doc.get_or_insert_map("blocks");
    assert!(map.get(&doc.transact(), &ids_before[1]).is_none());

    // 内容一样时什么都不改。
    let sv = doc.transact().state_vector();
    update_item(&doc, &heading, "甲（新）", &after[0].body, &mut SeqIds(200)).unwrap();
    assert_eq!(doc.transact().state_vector(), sv);

    // 正文里不能有一级、二级标题；不是标题块的 id 不行。
    assert_eq!(
        update_item(
            &doc,
            &heading,
            "甲",
            &to_blocks("## 混进来"),
            &mut SeqIds(300)
        ),
        Err(Error::HeadingInBody)
    );
    assert_eq!(
        update_item(&doc, &ids_after[0], "甲", &[], &mut SeqIds(300)),
        Err(Error::NotHeading(ids_after[0].clone()))
    );
    assert!(matches!(
        update_item(&doc, "nope", "甲", &[], &mut SeqIds(300)),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn items_are_inserted_and_removed() {
    let doc = two_items();
    let first = items(&doc)[0].heading_id.clone().unwrap();
    let (new_heading, _) = insert_item(
        &doc,
        Some(&first),
        "丙",
        &to_blocks("丙的正文"),
        &mut SeqIds(100),
    )
    .unwrap();
    let titles: Vec<String> = items(&doc).into_iter().map(|i| i.title).collect();
    assert_eq!(titles, ["甲", "丙", "乙"]);
    insert_item(&doc, None, "丁", &[], &mut SeqIds(200)).unwrap();
    remove_item(&doc, &new_heading).unwrap();
    let titles: Vec<String> = items(&doc).into_iter().map(|i| i.title).collect();
    assert_eq!(titles, ["甲", "乙", "丁"]);
    let map = doc.get_or_insert_map("blocks");
    assert!(map.get(&doc.transact(), &new_heading).is_none());
}

/// 在某个块后面插一个 AFFiNE 里才有的块（比如图片），像 AFFiNE 自己写的那样。
fn add_foreign_block(doc: &Doc, after: &str, id: &str, flavour: &str) {
    let map = doc.get_or_insert_map("blocks");
    let mut txn = doc.transact_mut();
    let m: MapRef = map.insert(&mut txn, id, MapPrelim::default());
    m.insert(&mut txn, "sys:id", id);
    m.insert(&mut txn, "sys:flavour", flavour);
    m.insert(&mut txn, "sys:children", ArrayPrelim::default());
    m.insert(&mut txn, "prop:sourceId", "blob-1");
    let notes: Vec<ArrayRef> = map
        .iter(&txn)
        .filter_map(|(_, v)| match v {
            Out::YMap(b) => match (b.get(&txn, "sys:flavour"), b.get(&txn, "sys:children")) {
                (Some(Out::Any(Any::String(f))), Some(Out::YArray(a))) if &*f == "affine:note" => {
                    Some(a)
                }
                _ => None,
            },
            _ => None,
        })
        .collect();
    let children = &notes[0];
    let ids: Vec<String> = children
        .iter(&txn)
        .map(|v| match v {
            Out::Any(Any::String(s)) => s.to_string(),
            _ => String::new(),
        })
        .collect();
    let at = ids.iter().position(|x| x == after).unwrap() as u32 + 1;
    children.insert(&mut txn, at, id);
}

#[test]
fn blocks_only_in_affine_are_kept_through_their_placeholder() {
    let doc = two_items();
    let item = &items(&doc)[0];
    let heading = item.heading_id.clone().unwrap();
    add_foreign_block(&doc, &block_ids(item)[0], "img1", "affine:image");
    let item = items(&doc)[0].clone();
    let md = item.body_markdown();
    assert!(
        md.contains("<!-- affine block=img1 flavour=affine:image"),
        "{md}"
    );
    assert!(!item.lossy, "占位块本身不算损失");

    // git 那边改了别的段落、占位注释还在：图片留着。
    let body = to_blocks(&md.replace("第二段", "第二段改了"));
    update_item(&doc, &heading, "甲", &body, &mut SeqIds(100)).unwrap();
    let item = items(&doc)[0].clone();
    assert!(block_ids(&item).contains(&"img1".to_owned()));
    assert!(item.body_markdown().contains("第二段改了"));

    // 占位注释挪到最后：图片跟着挪。
    let md = item.body_markdown();
    let line = md.lines().find(|l| l.contains("img1")).unwrap().to_owned();
    let moved = format!("{}\n\n{line}", md.replace(&format!("{line}\n\n"), ""));
    update_item(&doc, &heading, "甲", &to_blocks(&moved), &mut SeqIds(200)).unwrap();
    let item = items(&doc)[0].clone();
    assert_eq!(block_ids(&item).last().map(String::as_str), Some("img1"));

    // 占位注释删掉了：图片也删掉。
    let without: String = to_blocks(&moved)
        .into_iter()
        .filter(|b| b.id.as_deref() != Some("img1"))
        .map(|b| to_markdown(&[b]))
        .collect::<Vec<_>>()
        .join("\n\n");
    update_item(&doc, &heading, "甲", &to_blocks(&without), &mut SeqIds(300)).unwrap();
    let map = doc.get_or_insert_map("blocks");
    assert!(map.get(&doc.transact(), "img1").is_none());

    // 指向不存在的块的占位注释跳过，报出来。
    let skipped = update_item(
        &doc,
        &heading,
        "甲",
        &to_blocks("<!-- affine block=gone flavour=affine:image -->"),
        &mut SeqIds(400),
    )
    .unwrap();
    assert_eq!(skipped, ["gone"]);
}

#[test]
fn formatting_markdown_cannot_hold_is_kept_when_the_block_is_unchanged() {
    let doc = two_items();
    let item = items(&doc)[0].clone();
    let heading = item.heading_id.clone().unwrap();
    let first = block_ids(&item)[0].clone();
    // AFFiNE 里给第一段加了下划线（Markdown 写不出来）。
    {
        let map = doc.get_or_insert_map("blocks");
        let mut txn = doc.transact_mut();
        let Some(Out::YMap(b)) = map.get(&txn, &first) else {
            panic!()
        };
        let Some(Out::YText(t)) = b.get(&txn, "prop:text") else {
            panic!()
        };
        let mut attrs = yrs::types::Attrs::new();
        attrs.insert("underline".into(), true.into());
        let len = t.len(&txn);
        t.format(&mut txn, 0, len, attrs);
    }
    let item = items(&doc)[0].clone();
    assert!(item.lossy);
    // git 那边只改了第二段：第一段（带下划线）原样留着。
    let body = to_blocks("第一段\n\n第二段改了");
    update_item(&doc, &heading, "甲", &body, &mut SeqIds(100)).unwrap();
    let item = items(&doc)[0].clone();
    assert_eq!(block_ids(&item)[0], first);
    assert!(item.lossy, "下划线还在");
}

#[test]
fn empty_paragraphs_stay_where_they_are() {
    let doc = two_items();
    let item = items(&doc)[0].clone();
    let heading = item.heading_id.clone().unwrap();
    // AFFiNE 里人在两段中间按了一下回车。
    {
        let map = doc.get_or_insert_map("blocks");
        let mut txn = doc.transact_mut();
        let m: MapRef = map.insert(&mut txn, "empty1", MapPrelim::default());
        m.insert(&mut txn, "sys:id", "empty1");
        m.insert(&mut txn, "sys:flavour", "affine:paragraph");
        m.insert(&mut txn, "sys:children", ArrayPrelim::default());
        m.insert(&mut txn, "prop:type", "text");
        let _: TextRef = m.insert(&mut txn, "prop:text", TextPrelim::new(""));
    }
    add_foreign_block_into_place(&doc, &block_ids(&item)[0], "empty1");
    let item = items(&doc)[0].clone();
    assert_eq!(item.body_markdown(), "第一段\n\n第二段");
    update_item(
        &doc,
        &heading,
        "甲",
        &to_blocks("第一段\n\n第二段改了"),
        &mut SeqIds(100),
    )
    .unwrap();
    let item = items(&doc)[0].clone();
    assert_eq!(block_ids(&item)[1], "empty1", "空段落留在原地");
    assert_eq!(item.body_markdown(), "第一段\n\n第二段改了");
}

/// 把已经在表里的块插到某个块后面。
fn add_foreign_block_into_place(doc: &Doc, after: &str, id: &str) {
    let map = doc.get_or_insert_map("blocks");
    let mut txn = doc.transact_mut();
    for (_, v) in map.iter(&txn).collect::<Vec<_>>() {
        let Out::YMap(b) = v else { continue };
        let Some(Out::YArray(a)) = b.get(&txn, "sys:children") else {
            continue;
        };
        let ids: Vec<String> = a
            .iter(&txn)
            .map(|v| match v {
                Out::Any(Any::String(s)) => s.to_string(),
                _ => String::new(),
            })
            .collect();
        if let Some(i) = ids.iter().position(|x| x == after) {
            a.insert(&mut txn, i as u32 + 1, id);
            return;
        }
    }
    panic!("找不到 {after}");
}

#[test]
fn nested_lists_and_edgeless_notes() {
    let doc = Doc::new();
    let md = "## 清单\n\n- 一\n  - 一点一\n    - 更深\n- 二\n\n1. 甲\n   - 甲的子项";
    create(&doc, "清单", &to_blocks(md), &mut SeqIds(0)).unwrap();
    let item = items(&doc)[0].clone();
    assert_eq!(
        item.body_markdown(),
        "- 一\n  - 一点一\n    - 更深\n- 二\n\n1. 甲\n   - 甲的子项"
    );
    // 白板上才有的 note 不算正文。
    {
        let map = doc.get_or_insert_map("blocks");
        let mut txn = doc.transact_mut();
        let page = map
            .iter(&txn)
            .find_map(|(_, v)| match v {
                Out::YMap(b) if matches!(b.get(&txn, "sys:flavour"), Some(Out::Any(Any::String(f))) if &*f == "affine:page") => Some(b),
                _ => None,
            })
            .unwrap();
        let n: MapRef = map.insert(&mut txn, "edge", MapPrelim::default());
        n.insert(&mut txn, "sys:id", "edge");
        n.insert(&mut txn, "sys:flavour", "affine:note");
        n.insert(&mut txn, "prop:displayMode", "edgeless");
        let kids: ArrayRef = n.insert(&mut txn, "sys:children", ArrayPrelim::default());
        kids.push_back(&mut txn, "b2");
        let Some(Out::YArray(pc)) = page.get(&txn, "sys:children") else {
            panic!()
        };
        pc.push_back(&mut txn, "edge");
    }
    assert_eq!(items(&doc).len(), 1);
}

#[test]
fn random_ids_look_like_affine_ids() {
    let mut ids = RandomIds;
    let a = ids.next_id();
    let b = ids.next_id();
    assert_eq!(a.len(), 10);
    assert_ne!(a, b);
    assert!(
        a.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    );
}

/// 和 AFFiNE 互通的检查，平时不跑（要外面的文件）：
///
/// - `AFFINE_DOC_FIXTURE=<AFFiNE 导出的文档快照>`：读出来、按条目切开、打印 Markdown；
/// - `AFFINE_DOC_DUMP=<路径>`：把 `SAMPLE` 写成一篇文档，Yjs 更新存到这个路径，给 yjs 去读。
#[test]
#[ignore]
fn interop_with_affine_files() {
    if let Ok(path) = std::env::var("AFFINE_DOC_FIXTURE") {
        let bytes = std::fs::read(&path).unwrap();
        let doc = Doc::new();
        doc.transact_mut()
            .apply_update(Update::decode_v1(&bytes).unwrap())
            .unwrap();
        let page = read(&doc).unwrap();
        println!("# {}\n", page.title);
        println!("{}", to_markdown(&page.blocks));
        for s in split(&page.blocks) {
            if let Section::Item(i) = s {
                println!("---- 条目「{}」 lossy={}", i.title, i.lossy);
            }
        }
    }
    if let Ok(path) = std::env::var("AFFINE_DOC_DUMP") {
        let doc = Doc::new();
        create(&doc, "流程", &to_blocks(SAMPLE), &mut RandomIds).unwrap();
        let update = doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        std::fs::write(path, update).unwrap();
    }
}
