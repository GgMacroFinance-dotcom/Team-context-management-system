//! Markdown 子集和块互相转换（`docs/affine-sync.md` 第 4 节）。
//!
//! 认得的：段落、一到六级标题、引用、无序、有序和待办列表（可以嵌套）、代码块、分隔线，
//! 行内的粗体、斜体、删除线、行内代码、链接。
//!
//! - Markdown 里别的写法（表格、图片、HTML、脚注……）转成块时尽量保住文字：表格整段放进代码块，
//!   图片变成链接，HTML 当普通文字；这些块标成有损失。
//! - 块里 Markdown 写不出的（段落的子块、别的行内格式）也标成有损失。
//! - 不转换的块写成一行占位注释 `<!-- affine block=<id> flavour=<类型> ... -->`，转回来还是那个块。
//! - 空段落写不出来，转成 Markdown 时略过。

use std::sync::LazyLock;

use comrak::nodes::{AstNode, ListType, NodeValue};
use regex::Regex;

use crate::model::{Block, Content, ListKind, Marks, ParagraphKind, Span, normalize_spans};

fn spans_eq(a: &[Span], b: &[Span]) -> bool {
    normalize_spans(a.to_vec()) == normalize_spans(b.to_vec())
}

/// 占位注释的样子。
static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^<!--\s*affine\s+block=([A-Za-z0-9_-]+)\s+flavour=([A-Za-z0-9:_-]+)")
        .expect("占位注释的正则")
});

/// 占位注释里给人看的一句。
const PLACEHOLDER_NOTE: &str = "这一块只在 AFFiNE 里，没有同步";

fn options() -> comrak::Options<'static> {
    let mut o = comrak::Options::default();
    o.extension.strikethrough = true;
    o.extension.tasklist = true;
    o.extension.table = true;
    // 中文里粗体挨着标点（`**“引号”**中文`）时 CommonMark 认不出来，开 CJK 友好的规则。
    o.extension.cjk_friendly_emphasis = true;
    o
}

// ---------------------------------------------------------------- Markdown → 块

/// Markdown 转成块。新块都没有 `id`，占位注释转成带原来 `id` 的 `Other`。
pub fn to_blocks(md: &str) -> Vec<Block> {
    let arena = comrak::Arena::new();
    let root = comrak::parse_document(&arena, md, &options());
    let lines: Vec<&str> = md.lines().collect();
    let mut out = Vec::new();
    for node in root.children() {
        convert_block(node, &lines, &mut out);
    }
    out
}

fn source_of<'a>(node: &'a AstNode<'a>, lines: &[&str]) -> String {
    let pos = node.data.borrow().sourcepos;
    let start = pos.start.line.max(1) - 1;
    let end = pos.end.line.max(1).min(lines.len());
    lines
        .get(start..end)
        .map(|l| l.join("\n"))
        .unwrap_or_default()
}

fn convert_block<'a>(node: &'a AstNode<'a>, lines: &[&str], out: &mut Vec<Block>) {
    let value = node.data.borrow().value.clone();
    match value {
        NodeValue::Paragraph => {
            let (text, lossy) = inline_of(node);
            let mut b = Block::paragraph(ParagraphKind::Text, text);
            b.lossy = lossy;
            out.push(b);
        }
        NodeValue::Heading(h) => {
            let (text, lossy) = inline_of(node);
            let mut b = Block::paragraph(ParagraphKind::Heading(h.level.clamp(1, 6)), text);
            b.lossy = lossy;
            out.push(b);
        }
        NodeValue::BlockQuote => {
            // AFFiNE 的引用是一种段落；引用里的每一段变成一个引用段落，别的照常转、标成有损失。
            for child in node.children() {
                if matches!(child.data.borrow().value, NodeValue::Paragraph) {
                    let (text, lossy) = inline_of(child);
                    let mut b = Block::paragraph(ParagraphKind::Quote, text);
                    b.lossy = lossy;
                    out.push(b);
                } else {
                    let from = out.len();
                    convert_block(child, lines, out);
                    for b in &mut out[from..] {
                        b.lossy = true;
                    }
                }
            }
        }
        NodeValue::List(list) => {
            for item in node.children() {
                out.push(convert_item(item, list.list_type, lines));
            }
        }
        NodeValue::CodeBlock(c) => {
            let language = c
                .info
                .split_whitespace()
                .next()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let mut text = c.literal.clone();
            if text.ends_with('\n') {
                text.pop();
            }
            out.push(Block::new(Content::Code { language, text }));
        }
        NodeValue::ThematicBreak(_) => out.push(Block::new(Content::Divider)),
        NodeValue::HtmlBlock(h) => {
            let literal = h.literal.trim_end_matches('\n');
            if let Some(c) = PLACEHOLDER.captures(literal) {
                out.push(Block {
                    id: Some(c[1].to_owned()),
                    content: Content::Other {
                        flavour: c[2].to_owned(),
                    },
                    lossy: false,
                });
            } else {
                let mut b = Block::paragraph(ParagraphKind::Text, vec![Span::plain(literal)]);
                b.lossy = true;
                out.push(b);
            }
        }
        _ => {
            // 表格等：原文放进代码块，文字不丢。
            let mut b = Block::new(Content::Code {
                language: Some("markdown".to_owned()),
                text: source_of(node, lines),
            });
            b.lossy = true;
            out.push(b);
        }
    }
}

fn convert_item<'a>(item: &'a AstNode<'a>, list_type: ListType, lines: &[&str]) -> Block {
    let kind = match &item.data.borrow().value {
        NodeValue::TaskItem(t) => ListKind::Todo {
            checked: t.symbol.is_some(),
        },
        _ => match list_type {
            ListType::Bullet => ListKind::Bulleted,
            ListType::Ordered => ListKind::Numbered,
        },
    };
    let mut text = Vec::new();
    let mut lossy = false;
    let mut children = Vec::new();
    for (i, child) in item.children().enumerate() {
        if i == 0 && matches!(child.data.borrow().value, NodeValue::Paragraph) {
            let (t, l) = inline_of(child);
            text = t;
            lossy |= l;
        } else {
            convert_block(child, lines, &mut children);
        }
    }
    Block {
        id: None,
        content: Content::List {
            kind,
            text,
            children,
        },
        lossy,
    }
}

fn inline_of<'a>(node: &'a AstNode<'a>) -> (Vec<Span>, bool) {
    let mut spans = Vec::new();
    let mut lossy = false;
    for child in node.children() {
        inline(child, &Marks::default(), &mut spans, &mut lossy);
    }
    (normalize_spans(spans), lossy)
}

fn inline<'a>(node: &'a AstNode<'a>, marks: &Marks, out: &mut Vec<Span>, lossy: &mut bool) {
    let value = node.data.borrow().value.clone();
    let push = |out: &mut Vec<Span>, text: &str, marks: &Marks| {
        out.push(Span {
            text: text.to_owned(),
            marks: marks.clone(),
        })
    };
    let nested = |m: Marks, out: &mut Vec<Span>, lossy: &mut bool| {
        for c in node.children() {
            inline(c, &m, out, lossy);
        }
    };
    match value {
        NodeValue::Text(t) => push(out, &t, marks),
        NodeValue::SoftBreak | NodeValue::LineBreak => push(out, "\n", marks),
        NodeValue::Code(c) => push(
            out,
            &c.literal,
            &Marks {
                code: true,
                ..marks.clone()
            },
        ),
        NodeValue::Emph(_) => nested(
            Marks {
                italic: true,
                ..marks.clone()
            },
            out,
            lossy,
        ),
        NodeValue::Strong(_) => nested(
            Marks {
                bold: true,
                ..marks.clone()
            },
            out,
            lossy,
        ),
        NodeValue::Strikethrough(_) => nested(
            Marks {
                strike: true,
                ..marks.clone()
            },
            out,
            lossy,
        ),
        NodeValue::Link(l) => nested(
            Marks {
                link: Some(l.url.clone()),
                ..marks.clone()
            },
            out,
            lossy,
        ),
        NodeValue::Image(l) => {
            // 图片这一版不转：留下说明文字，链到图片地址。
            *lossy = true;
            let m = Marks {
                link: Some(l.url.clone()),
                ..marks.clone()
            };
            let before = out.len();
            nested(m.clone(), out, lossy);
            if out.len() == before {
                push(out, &l.url, &m);
            }
        }
        NodeValue::HtmlInline(h) => {
            *lossy = true;
            push(out, &h, marks);
        }
        _ => {
            *lossy = true;
            nested(marks.clone(), out, lossy);
        }
    }
}

// ---------------------------------------------------------------- 块 → Markdown

/// 块转成 Markdown。相邻的同一种列表项挨着写（一个列表），别的块之间空一行。
pub fn to_markdown(blocks: &[Block]) -> String {
    let mut out = String::new();
    let mut prev: Option<Family> = None;
    let mut number = 0usize;
    for b in blocks {
        let Some(text) = render_block(b, &mut number, prev) else {
            continue;
        };
        let fam = family(b);
        if !out.is_empty() {
            out.push_str(if fam.is_some() && fam == prev {
                "\n"
            } else {
                "\n\n"
            });
        }
        out.push_str(&text);
        prev = fam;
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    /// `-` 开头的：无序和待办。
    Dash,
    Ordered,
}

fn family(b: &Block) -> Option<Family> {
    match &b.content {
        Content::List {
            kind: ListKind::Numbered,
            ..
        } => Some(Family::Ordered),
        Content::List { .. } => Some(Family::Dash),
        _ => None,
    }
}

/// 空段落返回 `None`（写不出来）。
fn render_block(b: &Block, number: &mut usize, prev: Option<Family>) -> Option<String> {
    Some(match &b.content {
        Content::Paragraph {
            kind,
            text,
            children,
        } => {
            let mut s = match kind {
                ParagraphKind::Text => render_lines(text),
                ParagraphKind::Heading(l) => {
                    let line = render_inline(text).replace('\n', " ");
                    format!("{} {}", "#".repeat(usize::from(*l)), line)
                        .trim_end()
                        .to_owned()
                }
                ParagraphKind::Quote => {
                    let body = render_lines(text);
                    if body.is_empty() {
                        ">".to_owned()
                    } else {
                        prefix_lines(&body, "> ", ">")
                    }
                }
            };
            if s.is_empty() && children.is_empty() {
                return None;
            }
            if !children.is_empty() {
                let rest = to_markdown(children);
                if !rest.is_empty() {
                    if !s.is_empty() {
                        s.push_str("\n\n");
                    }
                    s.push_str(&rest);
                }
            }
            s
        }
        Content::List {
            kind,
            text,
            children,
        } => {
            let marker = match kind {
                ListKind::Bulleted => "- ".to_owned(),
                ListKind::Todo { checked: false } => "- [ ] ".to_owned(),
                ListKind::Todo { checked: true } => "- [x] ".to_owned(),
                ListKind::Numbered => {
                    *number = if prev == Some(Family::Ordered) {
                        *number + 1
                    } else {
                        1
                    };
                    format!("{number}. ")
                }
            };
            let indent = if matches!(kind, ListKind::Numbered) {
                marker.len()
            } else {
                2
            };
            let pad = " ".repeat(indent);
            let body = render_lines(text);
            let mut s = marker.trim_end().to_owned();
            if !body.is_empty() {
                s.push(' ');
                s.push_str(&prefix_rest(&body, &pad));
            }
            if !children.is_empty() {
                let rest = to_markdown(children);
                if !rest.is_empty() {
                    let starts_with_list = children.first().and_then(family).is_some();
                    s.push_str(if starts_with_list && !body.is_empty() {
                        "\n"
                    } else {
                        "\n\n"
                    });
                    s.push_str(&prefix_lines(&rest, &pad, ""));
                }
            }
            s
        }
        Content::Code { language, text } => {
            let longest = longest_run(text, '`');
            let fence = "`".repeat(longest.max(2) + 1);
            let lang = language.as_deref().unwrap_or("");
            if text.is_empty() {
                format!("{fence}{lang}\n{fence}")
            } else {
                format!("{fence}{lang}\n{text}\n{fence}")
            }
        }
        Content::Divider => "---".to_owned(),
        Content::Other { flavour } => {
            let id = b.id.as_ref()?;
            format!("<!-- affine block={id} flavour={flavour} {PLACEHOLDER_NOTE} -->")
        }
    })
}

fn longest_run(s: &str, c: char) -> usize {
    let mut best = 0;
    let mut cur = 0;
    for ch in s.chars() {
        if ch == c {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

/// 每一行前面加上 `prefix`；空行加 `empty`。
fn prefix_lines(s: &str, prefix: &str, empty: &str) -> String {
    s.split('\n')
        .map(|l| {
            if l.is_empty() {
                empty.to_owned()
            } else {
                format!("{prefix}{l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 除第一行外每一行前面加上 `pad`。
fn prefix_rest(s: &str, pad: &str) -> String {
    let mut it = s.split('\n');
    let mut out = it.next().unwrap_or("").to_owned();
    for l in it {
        out.push('\n');
        if !l.is_empty() {
            out.push_str(pad);
        }
        out.push_str(l);
    }
    out
}

/// 段落的文字：行内格式，每一行开头会被当成块的写法时转义。
fn render_lines(spans: &[Span]) -> String {
    let s = render_inline(spans);
    s.split('\n')
        .map(escape_line_start)
        .collect::<Vec<_>>()
        .join("\n")
}

/// 一行开头的 `#`、`>`、`-`、`+`、`=`、`1.` 这类会被当成标题、引用、列表；前面的空格会被当成代码块。
fn escape_line_start(line: &str) -> String {
    let trimmed = line.trim_start_matches(' ');
    let lead = line.len() - trimmed.len();
    let mut out = "&#32;".repeat(lead);
    let first = trimmed.chars().next();
    match first {
        Some('#' | '>' | '-' | '+' | '=') => {
            out.push('\\');
            out.push_str(trimmed);
        }
        Some(c) if c.is_ascii_digit() => {
            let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
            let rest = &trimmed[digits..];
            if rest.starts_with('.') || rest.starts_with(')') {
                out.push_str(&trimmed[..digits]);
                out.push('\\');
                out.push_str(rest);
            } else {
                out.push_str(trimmed);
            }
        }
        _ => out.push_str(trimmed),
    }
    out
}

fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '*' | '_' | '`' | '[' | ']' | '~' | '<' | '&' | '!' | '|'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn render_inline(spans: &[Span]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < spans.len() {
        // 链接把相邻、同一个网址的几段包在一起。
        if let Some(url) = &spans[i].marks.link {
            let mut j = i;
            while j < spans.len() && spans[j].marks.link.as_ref() == Some(url) {
                j += 1;
            }
            let inner: String = spans[i..j].iter().map(render_span).collect();
            out.push_str(&format!("[{inner}]({})", escape_url(url)));
            i = j;
        } else {
            out.push_str(&render_span(&spans[i]));
            i += 1;
        }
    }
    out
}

fn escape_url(url: &str) -> String {
    if url.contains([' ', '(', ')', '<', '>']) {
        format!("<{}>", url.replace('<', "%3C").replace('>', "%3E"))
    } else {
        url.to_owned()
    }
}

/// 一段文字加上格式。格式标记要贴着文字，前后的空白放到标记外面。
fn render_span(s: &Span) -> String {
    if s.marks.code {
        return render_code_span(&s.text, &s.marks);
    }
    let body = s.text.trim_matches(|c: char| c == ' ' || c == '\n');
    if body.is_empty() || !(s.marks.bold || s.marks.italic || s.marks.strike) {
        return escape_text(&s.text);
    }
    let start = s.text.find(body).unwrap_or(0);
    let (lead, rest) = s.text.split_at(start);
    let trail = &rest[body.len()..];
    let mut inner = escape_text(body);
    if s.marks.italic {
        inner = format!("*{inner}*");
    }
    if s.marks.bold {
        inner = format!("**{inner}**");
    }
    if s.marks.strike {
        inner = format!("~~{inner}~~");
    }
    format!("{}{inner}{}", escape_text(lead), escape_text(trail))
}

fn render_code_span(text: &str, marks: &Marks) -> String {
    let ticks = "`".repeat(longest_run(text, '`') + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') || text.starts_with(' ') {
        " "
    } else {
        ""
    };
    let mut inner = format!("{ticks}{pad}{text}{pad}{ticks}");
    if marks.italic {
        inner = format!("*{inner}*");
    }
    if marks.bold {
        inner = format!("**{inner}**");
    }
    if marks.strike {
        inner = format!("~~{inner}~~");
    }
    inner
}

/// 比较用的样子：单独转成 Markdown；不转换的块按 `id` 比。
pub(crate) fn key(b: &Block) -> String {
    match &b.content {
        Content::Other { flavour } => {
            format!("\u{0}other:{}:{flavour}", b.id.as_deref().unwrap_or(""))
        }
        _ => {
            let mut n = 0;
            render_block(b, &mut n, None).unwrap_or_default()
        }
    }
}

/// 转成 Markdown 再转回来，内容和原来一样（不管 `id` 和有没有损失，空段落不算）。
/// 有损失的块转回来本来就不一样，这里只查转换本身有没有出错。
pub fn roundtrips(blocks: &[Block]) -> bool {
    let back = to_blocks(&to_markdown(blocks));
    same_content(&back, blocks)
}

/// 两组块内容一样（不管 `id`、有没有损失，空段落不算）。
pub fn same_content(a: &[Block], b: &[Block]) -> bool {
    let a: Vec<&Block> = a.iter().filter(|x| !is_empty_paragraph(x)).collect();
    let b: Vec<&Block> = b.iter().filter(|x| !is_empty_paragraph(x)).collect();
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| block_eq(x, y))
}

fn block_eq(a: &Block, b: &Block) -> bool {
    match (&a.content, &b.content) {
        (
            Content::Paragraph {
                kind: k1,
                text: t1,
                children: c1,
            },
            Content::Paragraph {
                kind: k2,
                text: t2,
                children: c2,
            },
        ) => k1 == k2 && spans_eq(t1, t2) && same_content(c1, c2),
        (
            Content::List {
                kind: k1,
                text: t1,
                children: c1,
            },
            Content::List {
                kind: k2,
                text: t2,
                children: c2,
            },
        ) => k1 == k2 && spans_eq(t1, t2) && same_content(c1, c2),
        (Content::Other { flavour: f1 }, Content::Other { flavour: f2 }) => {
            f1 == f2 && a.id == b.id
        }
        (x, y) => x == y,
    }
}

/// 空的普通段落（Markdown 写不出来）。
pub(crate) fn is_empty_paragraph(b: &Block) -> bool {
    matches!(&b.content, Content::Paragraph { kind: ParagraphKind::Text, text, children }
        if children.is_empty() && text.iter().all(|s| s.text.is_empty()))
}
