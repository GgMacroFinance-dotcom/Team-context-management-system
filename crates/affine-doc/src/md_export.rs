//! 整篇文档导出成 Markdown（只导出，不导回来），相当于 AFFiNE 的“导出 → Markdown”。
//!
//! 和 [`crate::markdown`] 的分工：那边是同步用的 Markdown 子集，能来回转，认不得的块写成占位注释、
//! 留在 AFFiNE 里；这里从快照出发，尽量把每种块都写成人能读的 Markdown，给导出、备份、交给模型读用，
//! 写出来的不保证能转回同样的块。
//!
//! 写法照 BlockSuite 的 Markdown 导出（只读了设计，没复制代码）：
//!
//! - 文档标题写成一级标题；白板（`affine:surface`）和只在白板里显示的 note 不导出；
//! - 图片写成 `![标题](assets/<文件名>)`，附件写成 `[文件名](assets/<文件名>)`（BlockSuite 不导出附件，
//!   这里写成链接，不丢）；附件的文件名由调用方给，和导出包里 `assets/` 下的一样；
//! - 表格（`affine:table`）写成 GFM 表格，第一行当表头；
//! - 数据库（`affine:database`）写成 GFM 表格：表头是各列的名字，每行是一个子块，第一列是行的标题；
//!   单选、多选写选项的名字，日期写 `yyyy-MM-dd`（按 UTC），勾选了写 `✔`；
//! - 公式（`affine:latex`）写成 `$$` 块，行内公式写成 `$...$`；
//! - 提示块（`affine:callout`）写成引用，开头是它的 emoji；
//! - 书签和各种嵌入写成链接；引用别的文档（行内引用、嵌入文档卡片）写成 `[标题](<文档链接前缀>/<文档 id>)`，
//!   没给链接前缀时只写标题。
//!
//! 写不出来的（文字颜色、下划线、不认得的块……）记在 [`MdExport::lossy`] 里。

use std::collections::HashMap;

use serde_json::Value;

use crate::markdown::{
    escape_line_start, escape_text, longest_run, prefix_lines, prefix_rest, render_inline,
};
use crate::model::{Marks, Span, normalize_spans};

/// 导出时要的外部信息。
#[derive(Clone, Debug, Default)]
pub struct MdContext {
    /// 文档 id → 标题（根文档的文档列表里有），引用别的文档时用。
    pub titles: HashMap<String, String>,
    /// 文档链接的前缀，比如 `https://app.affine.pro/workspace/<工作区 id>`；`None` 时引用只写标题。
    pub doc_link_base: Option<String>,
    /// 附件 `blobId` → 导出里的文件名（比如 `<blobId>.png`），写成 `assets/<文件名>`。
    /// 没有的图片和附件不写，记进 [`MdExport::missing_assets`]。
    pub assets: HashMap<String, String>,
}

/// 导出的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MdExport {
    pub markdown: String,
    /// 写进去的附件 `blobId`，按出现的先后，不重复。
    pub used_assets: Vec<String>,
    /// 用到了、但 [`MdContext::assets`] 里没有的附件 `blobId`。
    pub missing_assets: Vec<String>,
    /// 写不出来、丢掉的东西，一条一句（比如“文字颜色”“块 affine:frame”），不重复。
    pub lossy: Vec<String>,
}

struct Writer<'a> {
    cx: &'a MdContext,
    out: MdExport,
}

const TEXT_ATTRS: [&str; 7] = [
    "bold",
    "italic",
    "strike",
    "code",
    "link",
    "reference",
    "latex",
];

/// 一段文字转出来的一小段：普通文字带格式，或者原样写出的 Markdown（行内公式）。
enum Piece {
    Span(Span),
    Raw(String),
}

impl Writer<'_> {
    fn lose(&mut self, what: impl Into<String>) {
        let what = what.into();
        if !self.out.lossy.contains(&what) {
            self.out.lossy.push(what);
        }
    }

    fn asset(&mut self, blob: &str) -> Option<String> {
        if blob.is_empty() {
            return None;
        }
        match self.cx.assets.get(blob) {
            Some(name) => {
                if !self.out.used_assets.iter().any(|b| b == blob) {
                    self.out.used_assets.push(blob.to_owned());
                }
                Some(format!("assets/{name}"))
            }
            None => {
                if !self.out.missing_assets.iter().any(|b| b == blob) {
                    self.out.missing_assets.push(blob.to_owned());
                }
                None
            }
        }
    }

    fn doc_title(&self, id: &str) -> String {
        self.cx
            .titles
            .get(id)
            .filter(|t| !t.is_empty())
            .cloned()
            .unwrap_or_else(|| "untitled".to_owned())
    }

    fn doc_url(&self, id: &str) -> Option<String> {
        self.cx
            .doc_link_base
            .as_ref()
            .map(|b| format!("{}/{id}", b.trim_end_matches('/')))
    }

    /// 快照里一段带格式的文字拆成一小段一小段。
    fn pieces(&mut self, text: Option<&Value>) -> Vec<Piece> {
        let Some(Value::Array(delta)) = text.and_then(|t| t.get("delta")) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for op in delta {
            let attrs = op.get("attributes").and_then(Value::as_object);
            let flag =
                |k: &str| attrs.and_then(|a| a.get(k)).and_then(Value::as_bool) == Some(true);
            let Some(insert) = op.get("insert").and_then(Value::as_str) else {
                self.lose("文字里嵌的东西");
                continue;
            };
            let mut text = insert.to_owned();
            let mut marks = Marks {
                bold: flag("bold"),
                italic: flag("italic"),
                strike: flag("strike"),
                code: flag("code"),
                link: attrs
                    .and_then(|a| a.get("link"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            };
            if let Some(page) = attrs
                .and_then(|a| a.get("reference"))
                .and_then(|r| r.get("pageId"))
                .and_then(Value::as_str)
            {
                text = self.doc_title(page);
                marks.link = self.doc_url(page);
            }
            if let Some(a) = attrs {
                for k in a.keys() {
                    if !TEXT_ATTRS.contains(&k.as_str()) && !a[k].is_null() {
                        self.lose(format!("文字格式 {k}"));
                    }
                }
            }
            match attrs.and_then(|a| a.get("latex")).and_then(Value::as_str) {
                // 行内公式原样写成 `$...$`，不能按普通文字转义。
                Some(latex) => out.push(Piece::Raw(format!("${latex}$"))),
                None => out.push(Piece::Span(Span { text, marks })),
            }
        }
        out
    }

    /// 写成一行行内 Markdown。
    fn inline(&mut self, text: Option<&Value>) -> String {
        let mut out = String::new();
        let mut run = Vec::new();
        for p in self.pieces(text) {
            match p {
                Piece::Span(s) => run.push(s),
                Piece::Raw(r) => {
                    out.push_str(&render_inline(&normalize_spans(std::mem::take(&mut run))));
                    out.push_str(&r);
                }
            }
        }
        out.push_str(&render_inline(&normalize_spans(run)));
        out
    }

    /// 段落正文：每一行开头会被当成块的写法时转义。
    fn lines(&mut self, text: Option<&Value>) -> String {
        self.inline(text)
            .split('\n')
            .map(escape_line_start)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 去掉格式的文字。
    fn plain(&mut self, text: Option<&Value>) -> String {
        self.pieces(text)
            .into_iter()
            .map(|p| match p {
                Piece::Span(s) => s.text,
                Piece::Raw(r) => r,
            })
            .collect()
    }

    /// 子块一个个写出来，块之间空一行；挨着的同一种列表项只换行（写成一个列表），
    /// 和 [`crate::markdown::to_markdown`] 一样。
    fn children_md(&mut self, b: &Value) -> String {
        let mut out = String::new();
        let mut prev = None;
        if let Some(Value::Array(children)) = b.get("children") {
            for c in children {
                let Some(s) = self.block(c) else { continue };
                let fam = list_family(c);
                if !out.is_empty() {
                    out.push_str(if fam.is_some() && fam == prev {
                        "\n"
                    } else {
                        "\n\n"
                    });
                }
                out.push_str(&s);
                prev = fam;
            }
        }
        out
    }

    /// 一个块写成 Markdown；什么都不写时返回 `None`。
    fn block(&mut self, b: &Value) -> Option<String> {
        let flavour = b.get("flavour").and_then(Value::as_str).unwrap_or_default();
        let props = b.get("props").cloned().unwrap_or(Value::Null);
        let prop = |k: &str| props.get(k);
        let s = |k: &str| {
            props
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let text = match flavour {
            "affine:page" => {
                let title = self.inline(prop("title"));
                let body = self.children_md(b);
                let head = if title.is_empty() {
                    String::new()
                } else {
                    format!("# {}", title.replace('\n', " ").trim_end())
                };
                join(&[head, body])
            }
            "affine:surface" => {
                let has_elements = prop("elements")
                    .and_then(Value::as_object)
                    .is_some_and(|m| !m.is_empty());
                let has_children = b
                    .get("children")
                    .and_then(Value::as_array)
                    .is_some_and(|c| !c.is_empty());
                if has_elements || has_children {
                    self.lose("白板");
                }
                return None;
            }
            "affine:note" => {
                if s("displayMode") == "edgeless" {
                    self.lose("只在白板里显示的 note");
                    return None;
                }
                self.children_md(b)
            }
            "affine:paragraph" => {
                let kind = s("type");
                let head = match kind.as_str() {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        let level = kind[1..].parse::<usize>().unwrap_or(1);
                        let line = self
                            .inline(prop("text"))
                            .replace('\n', " ")
                            .trim_end()
                            .to_owned();
                        if line.is_empty() {
                            String::new()
                        } else {
                            format!("{} {line}", "#".repeat(level))
                        }
                    }
                    "quote" => {
                        let body = self.lines(prop("text"));
                        if body.is_empty() {
                            String::new()
                        } else {
                            prefix_lines(&body, "> ", ">")
                        }
                    }
                    _ => self.lines(prop("text")),
                };
                let body = self.children_md(b);
                if !body.is_empty() {
                    self.lose("段落的子块（写在段落后面）");
                }
                join(&[head, body])
            }
            "affine:list" => {
                let marker = match s("type").as_str() {
                    "numbered" => {
                        let n = props.get("order").and_then(Value::as_u64).unwrap_or(1);
                        format!("{n}. ")
                    }
                    "todo" => {
                        if props.get("checked").and_then(Value::as_bool) == Some(true) {
                            "- [x] ".to_owned()
                        } else {
                            "- [ ] ".to_owned()
                        }
                    }
                    "toggle" => {
                        self.lose("折叠列表（写成无序列表）");
                        "- ".to_owned()
                    }
                    _ => "- ".to_owned(),
                };
                let pad = " ".repeat(if s("type") == "numbered" {
                    marker.len()
                } else {
                    2
                });
                let body = self.lines(prop("text"));
                let mut out = marker.trim_end().to_owned();
                if !body.is_empty() {
                    out.push(' ');
                    out.push_str(&prefix_rest(&body, &pad));
                }
                let rest = self.children_md(b);
                if !rest.is_empty() {
                    let starts_with_list = b
                        .get("children")
                        .and_then(Value::as_array)
                        .and_then(|c| c.first())
                        .and_then(list_family)
                        .is_some();
                    out.push_str(if starts_with_list && !body.is_empty() {
                        "\n"
                    } else {
                        "\n\n"
                    });
                    out.push_str(&prefix_lines(&rest, &pad, ""));
                }
                out
            }
            "affine:code" => {
                let code = self.plain(prop("text"));
                let fence = "`".repeat(longest_run(&code, '`').max(2) + 1);
                let lang = s("language");
                if code.is_empty() {
                    format!("{fence}{lang}\n{fence}")
                } else {
                    format!("{fence}{lang}\n{code}\n{fence}")
                }
            }
            "affine:divider" => "---".to_owned(),
            "affine:image" => {
                let path = self.asset(&s("sourceId"))?;
                format!("![{}]({path})", escape_alt(&s("caption")))
            }
            "affine:attachment" => {
                let path = self.asset(&s("sourceId"))?;
                let name = match s("name") {
                    n if n.is_empty() => "attachment".to_owned(),
                    n => n,
                };
                format!("[{}]({path})", escape_alt(&name))
            }
            "affine:table" => self.table(&props),
            "affine:database" => self.database(b, &props),
            "affine:latex" => {
                let latex = s("latex");
                format!("$$\n{latex}\n$$")
            }
            "affine:callout" => {
                let emoji = s("emoji");
                let body = self.children_md(b);
                let body = join(&[emoji, body]);
                if body.is_empty() {
                    return None;
                }
                prefix_lines(&body, "> ", ">")
            }
            "affine:bookmark"
            | "affine:embed-youtube"
            | "affine:embed-figma"
            | "affine:embed-github"
            | "affine:embed-loom"
            | "affine:embed-iframe" => {
                let url = s("url");
                if url.is_empty() {
                    return None;
                }
                let title = match s("title") {
                    t if t.is_empty() => url.clone(),
                    t => t,
                };
                format!("[{}]({})", escape_alt(&title), link_url(&url))
            }
            "affine:embed-linked-doc" | "affine:embed-synced-doc" => {
                let page = s("pageId");
                if page.is_empty() {
                    return None;
                }
                let title = escape_alt(&self.doc_title(&page));
                match self.doc_url(&page) {
                    Some(url) => format!("[{title}]({})", link_url(&url)),
                    None => title,
                }
            }
            other => {
                // frame、白板里的文字、AI 对话这类：有子块就写子块，别的丢掉。
                self.lose(format!("块 {other}"));
                self.children_md(b)
            }
        };
        (!text.is_empty()).then_some(text)
    }

    fn cell_text(&mut self, text: Option<&Value>) -> String {
        escape_cell(&self.inline(text))
    }

    fn table(&mut self, props: &Value) -> String {
        let sorted = |key: &str, id: &str| -> Vec<String> {
            let mut v: Vec<(String, String)> = props
                .get(key)
                .and_then(Value::as_object)
                .map(|m| {
                    m.values()
                        .map(|x| {
                            let get =
                                |k: &str| x.get(k).and_then(Value::as_str).unwrap_or_default();
                            (get("order").to_owned(), get(id).to_owned())
                        })
                        .collect()
                })
                .unwrap_or_default();
            v.sort();
            v.into_iter().map(|(_, id)| id).collect()
        };
        let rows = sorted("rows", "rowId");
        let cols = sorted("columns", "columnId");
        if cols.is_empty() {
            return String::new();
        }
        let mut lines: Vec<Vec<String>> = Vec::new();
        for r in &rows {
            let row = cols
                .iter()
                .map(|c| {
                    let cell = props
                        .get("cells")
                        .and_then(|cs| cs.get(format!("{r}:{c}")))
                        .and_then(|cell| cell.get("text"))
                        .cloned();
                    self.cell_text(cell.as_ref())
                })
                .collect();
            lines.push(row);
        }
        if lines.is_empty() {
            lines.push(vec![String::new(); cols.len()]);
        }
        gfm_table(&lines)
    }

    fn database(&mut self, b: &Value, props: &Value) -> String {
        let title = self.inline(props.get("title"));
        let columns: Vec<&Value> = props
            .get("columns")
            .and_then(Value::as_array)
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        let name =
            |c: &Value| escape_cell(c.get("name").and_then(Value::as_str).unwrap_or_default());
        let is_title = |c: &Value| c.get("type").and_then(Value::as_str) == Some("title");
        // 第一列是行的标题：有 title 列用它的名字，没有就叫 Title。
        let mut header = vec![
            columns
                .iter()
                .find(|c| is_title(c))
                .map(|c| name(c))
                .unwrap_or_else(|| "Title".to_owned()),
        ];
        let others: Vec<&Value> = columns.iter().copied().filter(|c| !is_title(c)).collect();
        header.extend(others.iter().map(|c| name(c)));
        let mut lines = vec![header];
        if let Some(Value::Array(children)) = b.get("children") {
            for row in children {
                let id = row.get("id").and_then(Value::as_str).unwrap_or_default();
                let mut line = vec![self.cell_text(row.pointer("/props/text"))];
                for c in &others {
                    let col = c.get("id").and_then(Value::as_str).unwrap_or_default();
                    let value = props
                        .get("cells")
                        .and_then(|cs| cs.get(id))
                        .and_then(|r| r.get(col))
                        .and_then(|cell| cell.get("value"));
                    line.push(self.db_value(c, value));
                }
                lines.push(line);
                if row
                    .get("children")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty())
                {
                    self.lose("数据库行里的子块");
                }
            }
        }
        let table = gfm_table(&lines);
        join(&[title, table])
    }

    fn db_value(&mut self, col: &Value, value: Option<&Value>) -> String {
        let Some(v) = value.filter(|v| !v.is_null()) else {
            return String::new();
        };
        let kind = col.get("type").and_then(Value::as_str).unwrap_or_default();
        let option = |id: &Value| -> String {
            col.pointer("/data/options")
                .and_then(Value::as_array)
                .and_then(|opts| {
                    opts.iter()
                        .find(|o| o.get("id") == Some(id))
                        .and_then(|o| o.get("value"))
                        .and_then(Value::as_str)
                })
                .unwrap_or_default()
                .to_owned()
        };
        if v.is_object() && v.get(crate::snapshot::TEXT_MARK).is_some() {
            return self.cell_text(Some(v));
        }
        let text = match (kind, v) {
            ("select", id) => option(id),
            ("multi-select", Value::Array(ids)) => ids
                .iter()
                .map(option)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(", "),
            // 勾选写成 ✔，不勾空着。
            ("checkbox", Value::Bool(b)) => return if *b { "✔" } else { "" }.to_owned(),
            ("date", Value::Number(n)) => n.as_f64().map(ymd).unwrap_or_default(),
            (_, Value::String(s)) => s.clone(),
            (_, Value::Number(n)) => n.to_string(),
            (_, Value::Bool(b)) => b.to_string(),
            (_, Value::Array(a)) => a
                .iter()
                .map(|x| {
                    x.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| x.to_string())
                })
                .collect::<Vec<_>>()
                .join(", "),
            (_, other) => {
                self.lose(format!("数据库 {kind} 列的值"));
                other.to_string()
            }
        };
        escape_cell(&escape_text(&text))
    }
}

/// 列表项属于哪一种：`-` 开头的（无序、待办、折叠）是 0，有序的是 1；不是列表项是 `None`。
fn list_family(b: &Value) -> Option<u8> {
    if b.get("flavour").and_then(Value::as_str) != Some("affine:list") {
        return None;
    }
    Some(u8::from(
        b.pointer("/props/type").and_then(Value::as_str) == Some("numbered"),
    ))
}

/// 毫秒时间戳 → `yyyy-MM-dd`（UTC）。
fn ymd(ms: f64) -> String {
    let days = (ms / 86_400_000.0).floor() as i64;
    // 公历日期，算法见 Howard Hinnant 的 days_from_civil 的逆运算。
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn join(parts: &[String]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 表格格子里：换行写成 `<br>`，`|` 已经在转义里了。
fn escape_cell(s: &str) -> String {
    s.replace('\n', "<br>")
}

/// 链接文字和图片说明里的 `[`、`]` 要转义。
fn escape_alt(s: &str) -> String {
    escape_text(s).replace('\n', " ")
}

fn link_url(url: &str) -> String {
    if url.contains([' ', '(', ')', '<', '>']) {
        format!("<{}>", url.replace('<', "%3C").replace('>', "%3E"))
    } else {
        url.to_owned()
    }
}

/// GFM 表格，第一行当表头。
fn gfm_table(rows: &[Vec<String>]) -> String {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
    let line = |r: &Vec<String>| {
        let mut cells: Vec<&str> = r.iter().map(String::as_str).collect();
        cells.resize(width, "");
        format!("| {} |", cells.join(" | "))
    };
    let mut out = vec![line(&rows[0]), format!("|{}", " --- |".repeat(width))];
    out.extend(rows[1..].iter().map(line));
    out.join("\n")
}

/// 把一篇文档的快照（[`crate::snapshot::to_snapshot`] 的结果，或导出包里的）导出成 Markdown。
pub fn snapshot_to_markdown(snapshot: &Value, cx: &MdContext) -> MdExport {
    let mut w = Writer {
        cx,
        out: MdExport::default(),
    };
    if let Some(root) = snapshot.get("blocks") {
        w.out.markdown = w.block(root).unwrap_or_default();
    }
    w.out
}
