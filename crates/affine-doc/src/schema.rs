//! 所有块的结构：AFFiNE 0.27.5 里有哪些块（flavour）、各自的版本号、能放在谁下面，以及一个不挑块的
//! 读法（[`read_tree`]）——每个块的 `sys:*` 和全部 `prop:*` 原样读出来，带格式的文字保留全部属性。
//!
//! [`crate::doc`] 只认得能转成 Markdown 的那几种块；这里认得全部，给 agent 看文档全貌、写之前和写之后
//! 检查结构（[`check_tree`]）用。这一步只读，不写这些块。
//!
//! 依据是 AFFiNE 仓库里 MIT 部分的块定义（`blocksuite/affine/model/src/blocks/*`、
//! `blocksuite/affine/blocks/surface`、`blocksuite/affine/blocks/data-view`，以及前端
//! `packages/frontend/core/src/blocksuite/ai/blocks` 里的两种 AI 块），只读了设计，没复制代码。

use std::collections::BTreeMap;

use yrs::types::ToJson as _;
use yrs::types::text::YChange;
use yrs::{Any, Array, Doc, Map, MapRef, Out, ReadTxn, Text, Transact};

/// 块在文档里的角色（BlockSuite 的 `metadata.role`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// 页面块，一篇文档一个。
    Root,
    /// 能装别的块的容器（note、白板、callout、数据库……）。
    Hub,
    /// 内容块。
    Content,
}

/// 一种块。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flavour {
    pub name: &'static str,
    /// `sys:version` 应该写的值。
    pub version: i64,
    pub role: Role,
    /// 能放在哪些块下面；空表示不限（BlockSuite 里没写 `parent` 的块）。`@root` 是页面块。
    pub parents: &'static [&'static str],
    /// 这种块的属性里哪些是带格式的文字（Y.Text）。
    pub text_props: &'static [&'static str],
    /// 只在白板里出现（坐标、层级这类属性才有意义）。
    pub edgeless_only: bool,
    /// 属性按 `prop:a.b.c` 拍平存（`affine:table`）。
    pub flat_data: bool,
}

const IN_TEXT: &[&str] = &[
    "affine:note",
    "affine:paragraph",
    "affine:list",
    "affine:edgeless-text",
];
const IN_NOTE_OR_SURFACE: &[&str] = &[
    "affine:note",
    "affine:surface",
    "affine:edgeless-text",
    "affine:paragraph",
    "affine:list",
];

const fn f(
    name: &'static str,
    version: i64,
    role: Role,
    parents: &'static [&'static str],
    text_props: &'static [&'static str],
) -> Flavour {
    Flavour {
        name,
        version,
        role,
        parents,
        text_props,
        edgeless_only: false,
        flat_data: false,
    }
}

const fn edgeless(mut x: Flavour) -> Flavour {
    x.edgeless_only = true;
    x
}

/// AFFiNE 0.27.5 的全部块。嵌入类（`affine:embed-*`）除了 iframe 都能放进 note 和白板。
pub const FLAVOURS: &[Flavour] = &[
    f("affine:page", 2, Role::Root, &[], &["title"]),
    f("affine:surface", 5, Role::Hub, &["@root"], &[]),
    f("affine:note", 1, Role::Hub, &["@root"], &[]),
    f(
        "affine:paragraph",
        1,
        Role::Content,
        &[
            "affine:note",
            "affine:database",
            "affine:paragraph",
            "affine:list",
            "affine:edgeless-text",
            "affine:callout",
            "affine:transcription",
        ],
        &["text"],
    ),
    f(
        "affine:list",
        1,
        Role::Content,
        &[
            "affine:note",
            "affine:database",
            "affine:list",
            "affine:paragraph",
            "affine:edgeless-text",
            "affine:callout",
        ],
        &["text"],
    ),
    f("affine:code", 1, Role::Content, IN_TEXT, &["text"]),
    f("affine:divider", 1, Role::Content, &[], &[]),
    f(
        "affine:callout",
        1,
        Role::Hub,
        &[
            "affine:note",
            "affine:database",
            "affine:paragraph",
            "affine:list",
            "affine:edgeless-text",
            "affine:transcription",
        ],
        &["text"],
    ),
    f(
        "affine:database",
        3,
        Role::Hub,
        &["affine:note"],
        &["title"],
    ),
    f("affine:data-view", 1, Role::Hub, &["affine:note"], &[]),
    Flavour {
        flat_data: true,
        ..f("affine:table", 1, Role::Content, &["affine:note"], &[])
    },
    f("affine:image", 1, Role::Content, &[], &[]),
    f(
        "affine:attachment",
        1,
        Role::Content,
        IN_NOTE_OR_SURFACE,
        &[],
    ),
    f("affine:bookmark", 1, Role::Content, IN_NOTE_OR_SURFACE, &[]),
    f("affine:latex", 1, Role::Content, IN_TEXT, &[]),
    f(
        "affine:surface-ref",
        1,
        Role::Content,
        &["affine:note", "affine:paragraph", "affine:list"],
        &[],
    ),
    edgeless(f(
        "affine:frame",
        1,
        Role::Content,
        &["affine:surface"],
        &["title"],
    )),
    edgeless(f(
        "affine:edgeless-text",
        1,
        Role::Hub,
        &["affine:surface"],
        &[],
    )),
    f("affine:embed-iframe", 1, Role::Content, &[], &[]),
    f(
        "affine:embed-figma",
        1,
        Role::Content,
        IN_NOTE_OR_SURFACE,
        &[],
    ),
    f(
        "affine:embed-github",
        1,
        Role::Content,
        IN_NOTE_OR_SURFACE,
        &[],
    ),
    f(
        "affine:embed-html",
        1,
        Role::Content,
        IN_NOTE_OR_SURFACE,
        &[],
    ),
    f(
        "affine:embed-linked-doc",
        1,
        Role::Content,
        IN_NOTE_OR_SURFACE,
        &[],
    ),
    f(
        "affine:embed-loom",
        1,
        Role::Content,
        IN_NOTE_OR_SURFACE,
        &[],
    ),
    f(
        "affine:embed-synced-doc",
        1,
        Role::Content,
        IN_NOTE_OR_SURFACE,
        &[],
    ),
    f(
        "affine:embed-youtube",
        1,
        Role::Content,
        IN_NOTE_OR_SURFACE,
        &[],
    ),
    f("affine:embed-ai-chat", 1, Role::Content, &[], &[]),
    f("affine:transcription", 1, Role::Hub, &[], &[]),
];

/// 按名字找一种块。
pub fn flavour(name: &str) -> Option<&'static Flavour> {
    FLAVOURS.iter().find(|f| f.name == name)
}

/// 一段带格式的文字：插入的内容和它的全部属性（不只是 Markdown 认得的那几个）。
#[derive(Clone, Debug, PartialEq)]
pub enum Delta {
    Text {
        text: String,
        attrs: BTreeMap<String, Any>,
    },
    /// 嵌进文字里的对象（行内引用别的文档、公式……）。
    Embed {
        value: Any,
        attrs: BTreeMap<String, Any>,
    },
}

/// 一个属性的值。
#[derive(Clone, Debug, PartialEq)]
pub enum Prop {
    /// Y.Text。
    Text(Vec<Delta>),
    /// Y.Map（嵌套的属性，比如数据库的 `cells`）。
    Map(BTreeMap<String, Prop>),
    /// Y.Array。
    List(Vec<Prop>),
    /// 普通值。
    Value(Any),
}

impl Prop {
    /// 去掉格式的文字；不是文字就是 `None`。
    pub fn plain_text(&self) -> Option<String> {
        match self {
            Self::Text(d) => Some(
                d.iter()
                    .filter_map(|x| match x {
                        Delta::Text { text, .. } => Some(text.as_str()),
                        Delta::Embed { .. } => None,
                    })
                    .collect(),
            ),
            Self::Value(Any::String(s)) => Some(s.to_string()),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Value(Any::String(s)) => Some(s),
            _ => None,
        }
    }
}

/// 原样读出来的一个块。
#[derive(Clone, Debug, PartialEq)]
pub struct RawBlock {
    pub id: String,
    pub flavour: String,
    pub version: Option<i64>,
    /// 去掉 `prop:` 前缀的属性。拍平存的块（`affine:table`）在这里已经按 `.` 拼回嵌套的样子。
    pub props: BTreeMap<String, Prop>,
    pub children: Vec<RawBlock>,
}

impl RawBlock {
    pub fn prop(&self, name: &str) -> Option<&Prop> {
        self.props.get(name)
    }

    /// 从这个块往下数，按先序走一遍。
    pub fn walk(&self) -> Vec<&RawBlock> {
        let mut out = vec![self];
        for c in &self.children {
            out.extend(c.walk());
        }
        out
    }
}

/// 嵌套最多读几层，防止坏数据里有环。
const MAX_DEPTH: usize = 64;

fn read_delta<T: ReadTxn>(txn: &T, t: &yrs::TextRef) -> Vec<Delta> {
    t.diff(txn, YChange::identity)
        .into_iter()
        .map(|d| {
            let attrs: BTreeMap<String, Any> = d
                .attributes
                .as_deref()
                .into_iter()
                .flatten()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect();
            match &d.insert {
                Out::Any(Any::String(s)) => Delta::Text {
                    text: s.to_string(),
                    attrs,
                },
                other => Delta::Embed {
                    value: other.to_json(txn),
                    attrs,
                },
            }
        })
        .collect()
}

fn read_prop<T: ReadTxn>(txn: &T, v: &Out, depth: usize) -> Prop {
    if depth >= MAX_DEPTH {
        return Prop::Value(Any::Undefined);
    }
    match v {
        Out::YText(t) => Prop::Text(read_delta(txn, t)),
        Out::YMap(m) => Prop::Map(
            m.iter(txn)
                .map(|(k, v)| (k.to_owned(), read_prop(txn, &v, depth + 1)))
                .collect(),
        ),
        Out::YArray(a) => Prop::List(a.iter(txn).map(|v| read_prop(txn, &v, depth + 1)).collect()),
        Out::Any(a) => Prop::Value(a.clone()),
        other => Prop::Value(other.to_json(txn)),
    }
}

/// 把 `a.b.c` 这样拍平的键拼回嵌套的 Map。
fn unflatten(flat: BTreeMap<String, Prop>) -> BTreeMap<String, Prop> {
    let mut out: BTreeMap<String, Prop> = BTreeMap::new();
    for (key, value) in flat {
        let mut parts = key.split('.').peekable();
        let mut cur = &mut out;
        while let Some(p) = parts.next() {
            if parts.peek().is_none() {
                cur.insert(p.to_owned(), value);
                break;
            }
            let entry = cur
                .entry(p.to_owned())
                .or_insert_with(|| Prop::Map(BTreeMap::new()));
            if !matches!(entry, Prop::Map(_)) {
                *entry = Prop::Map(BTreeMap::new());
            }
            let Prop::Map(next) = entry else {
                unreachable!()
            };
            cur = next;
        }
    }
    out
}

fn read_raw<T: ReadTxn>(blocks: &MapRef, txn: &T, id: &str, depth: usize) -> Option<RawBlock> {
    let Some(Out::YMap(m)) = blocks.get(txn, id) else {
        return None;
    };
    let mut flavour = String::new();
    let mut version = None;
    let mut child_ids = Vec::new();
    let mut props = BTreeMap::new();
    for (k, v) in m.iter(txn) {
        match k {
            "sys:flavour" => flavour = read_prop(txn, &v, 0).as_str().unwrap_or("").to_owned(),
            "sys:version" => {
                if let Out::Any(Any::Number(n)) = &v {
                    version = n.as_f64().map(|x| x as i64);
                }
            }
            "sys:children" => {
                if let Out::YArray(a) = &v {
                    child_ids = a
                        .iter(txn)
                        .filter_map(|c| match c {
                            Out::Any(Any::String(s)) => Some(s.to_string()),
                            _ => None,
                        })
                        .collect();
                }
            }
            _ => {
                if let Some(name) = k.strip_prefix("prop:") {
                    props.insert(name.to_owned(), read_prop(txn, &v, 0));
                }
            }
        }
    }
    if flavour::is_flat(&flavour) {
        props = unflatten(props);
    }
    let children = if depth >= MAX_DEPTH {
        Vec::new()
    } else {
        child_ids
            .iter()
            .filter_map(|c| read_raw(blocks, txn, c, depth + 1))
            .collect()
    };
    Some(RawBlock {
        id: id.to_owned(),
        flavour,
        version,
        props,
        children,
    })
}

mod flavour {
    pub(super) fn is_flat(name: &str) -> bool {
        super::flavour(name).is_some_and(|f| f.flat_data)
    }
}

/// 从页面块往下读整棵块树（包括白板和只在白板里显示的 note）。没有页面块就是 `None`。
pub fn read_tree(doc: &Doc) -> Option<RawBlock> {
    let blocks = doc.get_or_insert_map("blocks");
    let txn = doc.transact();
    let page = blocks.iter(&txn).find_map(|(id, v)| match v {
        Out::YMap(m) => matches!(m.get(&txn, "sys:flavour"), Some(Out::Any(Any::String(s))) if &*s == "affine:page")
            .then(|| id.to_owned()),
        _ => None,
    })?;
    read_raw(&blocks, &txn, &page, 0)
}

/// 结构上的问题。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// 不认得的块（可能是更新的 AFFiNE 加的）。不一定是错，写的时候原样留着。
    UnknownFlavour { id: String, flavour: String },
    /// 版本号和 0.27.5 的不一样。
    Version {
        id: String,
        flavour: String,
        found: Option<i64>,
        expected: i64,
    },
    /// 放在了不该放的地方。
    Parent {
        id: String,
        flavour: String,
        parent: String,
    },
    /// 该是文字的属性不是文字。
    NotText {
        id: String,
        flavour: String,
        prop: String,
    },
}

/// 检查一棵块树合不合 AFFiNE 0.27.5 的规矩。写完一篇文档读回来查一遍，能挡住写坏结构的错。
pub fn check_tree(root: &RawBlock) -> Vec<Problem> {
    let mut out = Vec::new();
    check(root, None, &mut out);
    out
}

fn check(b: &RawBlock, parent: Option<&RawBlock>, out: &mut Vec<Problem>) {
    match flavour(&b.flavour) {
        None => out.push(Problem::UnknownFlavour {
            id: b.id.clone(),
            flavour: b.flavour.clone(),
        }),
        Some(f) => {
            if b.version != Some(f.version) {
                out.push(Problem::Version {
                    id: b.id.clone(),
                    flavour: b.flavour.clone(),
                    found: b.version,
                    expected: f.version,
                });
            }
            let parent_name = match parent {
                None => None,
                Some(p) if p.flavour == "affine:page" => Some("@root"),
                Some(p) => Some(p.flavour.as_str()),
            };
            let allowed = match parent_name {
                None => f.role == Role::Root,
                Some(p) => f.parents.is_empty() || f.parents.contains(&p),
            };
            if !allowed {
                out.push(Problem::Parent {
                    id: b.id.clone(),
                    flavour: b.flavour.clone(),
                    parent: parent.map(|p| p.flavour.clone()).unwrap_or_default(),
                });
            }
            for name in f.text_props {
                if let Some(p) = b.props.get(*name)
                    && !matches!(p, Prop::Text(_))
                {
                    out.push(Problem::NotText {
                        id: b.id.clone(),
                        flavour: b.flavour.clone(),
                        prop: (*name).to_owned(),
                    });
                }
            }
        }
    }
    for c in &b.children {
        check(c, Some(b), out);
    }
}
