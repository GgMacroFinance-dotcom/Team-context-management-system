//! 块的模型：认得的几种 AFFiNE 块（TCMS `docs/affine-sync.md` 第 4 节），别的一律当成“不转换的块”原样留着。

/// 行内格式。AFFiNE 里是 Y.Text 上的属性：`bold`、`italic`、`strike`、`code` 是 `true`，`link` 是网址。
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Marks {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub link: Option<String>,
}

/// 一段格式相同的文字。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Span {
    pub text: String,
    pub marks: Marks,
}

impl Span {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            marks: Marks::default(),
        }
    }
}

/// 段落的类型（`affine:paragraph` 的 `prop:type`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ParagraphKind {
    Text,
    Heading(u8),
    Quote,
}

impl ParagraphKind {
    pub(crate) fn as_affine(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Quote => "quote",
            Self::Heading(1) => "h1",
            Self::Heading(2) => "h2",
            Self::Heading(3) => "h3",
            Self::Heading(4) => "h4",
            Self::Heading(5) => "h5",
            Self::Heading(_) => "h6",
        }
    }

    pub(crate) fn from_affine(s: &str) -> Option<Self> {
        Some(match s {
            "text" => Self::Text,
            "quote" => Self::Quote,
            "h1" => Self::Heading(1),
            "h2" => Self::Heading(2),
            "h3" => Self::Heading(3),
            "h4" => Self::Heading(4),
            "h5" => Self::Heading(5),
            "h6" => Self::Heading(6),
            _ => return None,
        })
    }
}

/// 列表项的类型（`affine:list` 的 `prop:type` 和 `prop:checked`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ListKind {
    Bulleted,
    Numbered,
    Todo { checked: bool },
}

/// 一个块的内容。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Content {
    /// `affine:paragraph`。AFFiNE 里段落也能有子块（按 Tab 缩进），Markdown 里写不出来，
    /// 转成 Markdown 时子块跟在后面，并且标成转换有损失。
    Paragraph {
        kind: ParagraphKind,
        text: Vec<Span>,
        children: Vec<Block>,
    },
    /// `affine:list`：一个列表项，子块是缩进的下一级。
    List {
        kind: ListKind,
        text: Vec<Span>,
        children: Vec<Block>,
    },
    /// `affine:code`。代码里的格式不要。
    Code {
        language: Option<String>,
        text: String,
    },
    /// `affine:divider`。
    Divider,
    /// 不转换的块（图片、表格、数据库、嵌入……）：原样留在 AFFiNE 里，Markdown 里写成一行占位注释。
    Other { flavour: String },
}

/// 一个块。从 AFFiNE 读出来的带 `id`；从 Markdown 转出来的新块没有 `id`（写进去时分配），
/// 占位注释转回来的 `Other` 带着原来那个块的 `id`。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Block {
    pub id: Option<String>,
    pub content: Content,
    /// 转换时丢了东西：AFFiNE 里有、Markdown 写不出的格式或结构，或者 Markdown 里有、
    /// 这一版不转的写法（表格、图片、HTML……）。
    pub lossy: bool,
}

impl Block {
    pub fn new(content: Content) -> Self {
        Self {
            id: None,
            content,
            lossy: false,
        }
    }

    pub fn paragraph(kind: ParagraphKind, text: Vec<Span>) -> Self {
        Self::new(Content::Paragraph {
            kind,
            text,
            children: Vec::new(),
        })
    }

    /// 这一块和子块有没有转换损失。
    pub fn is_lossy(&self) -> bool {
        self.lossy
            || match &self.content {
                Content::Paragraph { children, .. } | Content::List { children, .. } => {
                    children.iter().any(Block::is_lossy)
                }
                _ => false,
            }
    }

    /// 一级或二级标题（切条目用）。
    pub(crate) fn heading_level(&self) -> Option<u8> {
        match &self.content {
            Content::Paragraph {
                kind: ParagraphKind::Heading(l),
                ..
            } => Some(*l),
            _ => None,
        }
    }
}

/// 去掉格式的文字。
pub fn plain_text(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

/// 整理成一种写法：粗体、斜体、删除线前后的空白挪到外面（看起来一样，Markdown 只能这样写），
/// 合并相邻、格式一样的段，去掉空段。
pub(crate) fn normalize_spans(spans: Vec<Span>) -> Vec<Span> {
    let mut pieces: Vec<Span> = Vec::with_capacity(spans.len());
    for s in spans {
        let m = &s.marks;
        if m.code || !(m.bold || m.italic || m.strike) {
            pieces.push(s);
            continue;
        }
        let ws = |c: char| c == ' ' || c == '\n';
        let body = s.text.trim_matches(ws);
        let outside = Marks {
            link: m.link.clone(),
            ..Marks::default()
        };
        if body.is_empty() {
            pieces.push(Span {
                text: s.text,
                marks: outside,
            });
            continue;
        }
        let start = s.text.len() - s.text.trim_start_matches(ws).len();
        let end = start + body.len();
        pieces.push(Span {
            text: s.text[..start].to_owned(),
            marks: outside.clone(),
        });
        pieces.push(Span {
            text: body.to_owned(),
            marks: s.marks.clone(),
        });
        pieces.push(Span {
            text: s.text[end..].to_owned(),
            marks: outside,
        });
    }
    let mut out: Vec<Span> = Vec::with_capacity(pieces.len());
    for s in pieces {
        if s.text.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.marks == s.marks => last.text.push_str(&s.text),
            _ => out.push(s),
        }
    }
    out
}
