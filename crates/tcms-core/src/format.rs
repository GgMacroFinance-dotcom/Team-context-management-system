//! 上下文仓库里 Markdown 文件的格式（`docs/architecture.md` 第 2 节）。
//!
//! - 第一层目录是范围：`org/`、`team-x/`、`role-x/`、`project-x/`。
//! - 文件开头可以有 YAML（`---` 包住），写这个文件里条目的默认值：`kind`、`tags`、`owner`、`review_by`。
//! - 每个二级标题是一条。文件里没有二级标题时，整个文件是一条，标题取一级标题或文件名。
//! - 标题下第一行非空行可以是 `<!-- ctx id=... kind=... -->`，写 id 和这一条自己的属性。

use std::collections::HashSet;

use comrak::nodes::NodeValue;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{ItemId, Kind, Scope, valid_date};

/// 解析时发现的问题。不会让整次索引失败，只跳过有问题的文件或条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Warning {
    pub path: String,
    /// 从 1 开始的行号。
    pub line: Option<usize>,
    pub message: String,
}

/// 从文件里解析出的一条。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedItem {
    /// 还没分配 id 时为空，要先 `tcms assign-ids`。
    pub id: Option<ItemId>,
    pub scope: Scope,
    pub kind: Kind,
    pub title: String,
    /// Markdown 正文，去掉了前后空行，换行统一成 `\n`。
    pub body: String,
    pub tags: Vec<String>,
    pub owner: Option<String>,
    pub review_by: Option<String>,
    pub supersedes: Option<ItemId>,
    /// 仓库里的相对路径，用 `/` 分隔。
    pub path: String,
    /// 标题所在的行（从 1 开始）；没有标题的单条文件是正文第一行。
    pub line: usize,
    /// `<!-- ctx ... -->` 所在行（从 0 开始），补 id 时用。
    pub meta_line: Option<usize>,
    /// 没有注释时，新注释插在这一行之前（从 0 开始）。
    pub insert_at: usize,
}

/// 参与内容哈希的字段。路径不算内容，单独比较。
#[derive(Serialize)]
struct HashedContent<'a> {
    scope: String,
    kind: &'static str,
    title: &'a str,
    body: &'a str,
    tags: &'a [String],
    owner: Option<&'a str>,
    review_by: Option<&'a str>,
    supersedes: Option<&'a str>,
}

impl ParsedItem {
    /// 内容哈希（SHA-256 十六进制）。内容变了版本号才加一。
    pub fn content_hash(&self) -> String {
        let content = HashedContent {
            scope: self.scope.to_string(),
            kind: self.kind.as_str(),
            title: &self.title,
            body: &self.body,
            tags: &self.tags,
            owner: self.owner.as_deref(),
            review_by: self.review_by.as_deref(),
            supersedes: self.supersedes.as_ref().map(ItemId::as_str),
        };
        let json = serde_json::to_vec(&content).expect("序列化内存里的结构不会失败");
        let digest = Sha256::digest(&json);
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[derive(Debug, Default)]
pub struct ParsedFile {
    pub items: Vec<ParsedItem>,
    pub warnings: Vec<Warning>,
}

/// 这个路径是不是上下文仓库里的条目文件；是的话返回它的范围。
/// 只认第一层目录是范围、扩展名是 `.md` 的文件，根目录下的文件（README、teams.yaml）不算。
pub fn scope_of_path(path: &str) -> Option<Scope> {
    let (dir, rest) = path.split_once('/')?;
    if rest.is_empty() || !path.ends_with(".md") {
        return None;
    }
    Scope::from_dir(dir)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrontMatter {
    kind: Option<String>,
    tags: Option<Vec<String>>,
    owner: Option<String>,
    review_by: Option<String>,
}

/// `<!-- ctx ... -->` 里写的属性。
#[derive(Debug, Default, PartialEq, Eq)]
struct Meta {
    id: Option<String>,
    kind: Option<String>,
    tags: Option<String>,
    owner: Option<String>,
    review_by: Option<String>,
    supersedes: Option<String>,
}

/// 去掉行尾的换行。
fn content(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r'])
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// 解析一个条目文件。`path` 必须是 [`scope_of_path`] 认得的路径。
pub fn parse_file(path: &str, text: &str) -> ParsedFile {
    let mut out = ParsedFile::default();
    let Some(scope) = scope_of_path(path) else {
        return out;
    };
    let warn = |out: &mut ParsedFile, line: Option<usize>, message: String| {
        out.warnings.push(Warning {
            path: path.to_owned(),
            line,
            message,
        });
    };

    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();

    // 文件开头的 YAML。
    let mut body_start = 0;
    let mut defaults = FrontMatter::default();
    if lines
        .first()
        .is_some_and(|l| content(l).trim_end() == "---")
    {
        let close = lines
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, l)| matches!(content(l).trim_end(), "---" | "..."))
            .map(|(i, _)| i);
        match close {
            Some(close) => {
                let yaml: String = lines[1..close].concat();
                if !yaml.trim().is_empty() {
                    match serde_saphyr::from_str::<FrontMatter>(&yaml) {
                        Ok(fm) => defaults = fm,
                        Err(e) => {
                            warn(
                                &mut out,
                                Some(1),
                                format!("文件开头的 YAML 解析不了，整个文件跳过：{e}"),
                            );
                            return out;
                        }
                    }
                }
                body_start = close + 1;
            }
            None => warn(
                &mut out,
                Some(1),
                "文件开头的 `---` 没有对应的结束行，当作正文处理".to_owned(),
            ),
        }
    }

    // 找出文档最外层的一级、二级标题。代码块、引用、列表里的不算。
    struct Heading {
        level: u8,
        start: usize,
        end: usize,
        setext: bool,
    }
    let body_text: String = lines[body_start..].concat();
    let arena = comrak::Arena::new();
    let root = comrak::parse_document(&arena, &body_text, &comrak::Options::default());
    let mut headings = Vec::new();
    for node in root.children() {
        let ast = node.data.borrow();
        if let NodeValue::Heading(h) = &ast.value
            && h.level <= 2
        {
            let start = body_start + ast.sourcepos.start.line.max(1) - 1;
            let end = (body_start + ast.sourcepos.end.line.max(1) - 1).max(start);
            headings.push(Heading {
                level: h.level,
                start,
                end: end.min(lines.len().saturating_sub(1)),
                setext: h.setext,
            });
        }
    }

    let title_of = |h: &Heading| -> String {
        if h.setext {
            lines[h.start..h.end]
                .iter()
                .map(|l| content(l).trim())
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            atx_title(content(lines[h.start]))
        }
    };

    // 每条的范围：(标题, 标题结束后的第一行, 这一条结束的行（不含）)。
    let mut segments: Vec<(Option<&Heading>, usize, usize)> = Vec::new();
    let h2: Vec<&Heading> = headings.iter().filter(|h| h.level == 2).collect();
    if h2.is_empty() {
        let first_h1 = headings.iter().find(|h| h.level == 1);
        let start = first_h1.map_or(body_start, |h| h.end + 1);
        // 一级标题之前还有内容的话，也算进正文。
        if let Some(h) = first_h1
            && lines[body_start..h.start].iter().any(|l| !is_blank(l))
        {
            warn(
                &mut out,
                Some(body_start + 1),
                "一级标题之前的内容没有进正文".to_owned(),
            );
        }
        segments.push((first_h1, start, lines.len()));
    } else {
        let mut stray: Option<usize> = None;
        let mut cursor = body_start;
        for (i, h) in headings.iter().enumerate() {
            // 不属于任何一条的行：第一个二级标题之前、一级标题之后到下一个二级标题之前。
            if h.level == 1 || cursor < h.start {
                let upto = h.start;
                if stray.is_none()
                    && let Some(off) = lines[cursor.min(upto)..upto]
                        .iter()
                        .position(|l| !is_blank(l))
                {
                    stray = Some(cursor + off);
                }
            }
            if h.level == 2 {
                let end = headings.get(i + 1).map_or(lines.len(), |n| n.start);
                segments.push((Some(h), h.end + 1, end));
                cursor = end;
            } else {
                cursor = h.end + 1;
            }
        }
        if let Some(line) = stray {
            warn(
                &mut out,
                Some(line + 1),
                "不在任何二级标题下面的内容不算条目，没有进索引".to_owned(),
            );
        }
    }

    let file_tags = defaults.tags.as_deref().map(normalize_tags);
    let file_kind = match defaults.kind.as_deref().map(str::parse::<Kind>) {
        Some(Ok(k)) => Some(k),
        Some(Err(e)) => {
            warn(
                &mut out,
                Some(1),
                format!("文件开头的 kind 不对，整个文件跳过：{e}"),
            );
            return out;
        }
        None => None,
    };

    for (heading, start, end) in segments {
        let title = match heading {
            Some(h) => title_of(h),
            None => file_stem(path),
        };
        let line_no = heading.map_or(start + 1, |h| h.start + 1);
        if title.is_empty() {
            warn(&mut out, Some(line_no), "标题是空的，这一条跳过".to_owned());
            continue;
        }

        // 标题下第一行非空行是不是 `<!-- ctx ... -->`。
        let first = (start..end).find(|&i| !is_blank(lines[i]));
        let mut meta = Meta::default();
        let mut meta_line = None;
        let mut body_from = start;
        if let Some(i) = first
            && let Some(parsed) = parse_meta(content(lines[i]))
        {
            match parsed {
                Ok(m) => {
                    meta = m;
                    meta_line = Some(i);
                    body_from = i + 1;
                }
                Err(e) => {
                    warn(
                        &mut out,
                        Some(i + 1),
                        format!("ctx 注释写法不对，这一条跳过：{e}"),
                    );
                    continue;
                }
            }
        }

        let id = match meta.id.as_deref().map(str::parse::<ItemId>) {
            Some(Ok(id)) => Some(id),
            Some(Err(e)) => {
                warn(
                    &mut out,
                    meta_line.map(|l| l + 1),
                    format!("{e}，这一条跳过"),
                );
                continue;
            }
            None => None,
        };
        let kind = match meta.kind.as_deref().map(str::parse::<Kind>) {
            Some(Ok(k)) => k,
            Some(Err(e)) => {
                warn(
                    &mut out,
                    meta_line.map(|l| l + 1),
                    format!("{e}，这一条跳过"),
                );
                continue;
            }
            None => match file_kind {
                Some(k) => k,
                None => {
                    warn(
                        &mut out,
                        Some(line_no),
                        "没有写 kind，按 reference（资料）处理".to_owned(),
                    );
                    Kind::Reference
                }
            },
        };
        let review_by = match meta.review_by.or_else(|| defaults.review_by.clone()) {
            Some(d) if valid_date(&d) => Some(d),
            Some(d) => {
                warn(
                    &mut out,
                    Some(line_no),
                    format!("review_by 不是有效日期（YYYY-MM-DD）：{d}，忽略这个属性"),
                );
                None
            }
            None => None,
        };
        let supersedes = match meta.supersedes.as_deref().map(str::parse::<ItemId>) {
            Some(Ok(s)) if Some(&s) == id.as_ref() => {
                warn(
                    &mut out,
                    Some(line_no),
                    "supersedes 不能指向自己，忽略这个属性".to_owned(),
                );
                None
            }
            Some(Ok(s)) => Some(s),
            Some(Err(e)) => {
                warn(
                    &mut out,
                    Some(line_no),
                    format!("supersedes：{e}，忽略这个属性"),
                );
                None
            }
            None => None,
        };
        let tags = match meta.tags {
            Some(t) => normalize_tags(&t.split(',').map(str::to_owned).collect::<Vec<_>>()),
            None => file_tags.clone().unwrap_or_default(),
        };
        let owner = meta
            .owner
            .or_else(|| defaults.owner.clone())
            .map(|o| o.trim().to_owned())
            .filter(|o| !o.is_empty());

        out.items.push(ParsedItem {
            id,
            scope: scope.clone(),
            kind,
            title,
            body: body_text_of(&lines[body_from.min(end)..end]),
            tags,
            owner,
            review_by,
            supersedes,
            path: path.to_owned(),
            line: line_no,
            meta_line,
            insert_at: start,
        });
    }
    out
}

/// ATX 标题的文字：去掉开头的 `#`、结尾可选的 `#`。
fn atx_title(line: &str) -> String {
    let t = line.trim_start().trim_start_matches('#').trim();
    // 结尾的 `#` 只有前面是空白时才算关闭符。
    let without = t.trim_end_matches('#');
    if without.len() < t.len() && (without.is_empty() || without.ends_with([' ', '\t'])) {
        without.trim_end().to_owned()
    } else {
        t.to_owned()
    }
}

/// 文件名去掉目录和 `.md`。
fn file_stem(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".md").unwrap_or(name).trim().to_owned()
}

/// 去掉前后的空行，换行统一成 `\n`，末尾不留换行。
fn body_text_of(lines: &[&str]) -> String {
    let first = lines.iter().position(|l| !is_blank(l));
    let last = lines.iter().rposition(|l| !is_blank(l));
    match (first, last) {
        (Some(a), Some(b)) => lines[a..=b]
            .iter()
            .map(|l| content(l))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    tags.iter()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty() && seen.insert(t.clone()))
        .collect()
}

/// 解析 `<!-- ctx key=value ... -->`。不是 ctx 注释时返回 `None`。
fn parse_meta(line: &str) -> Option<Result<Meta, String>> {
    let inner = line
        .trim()
        .strip_prefix("<!--")?
        .strip_suffix("-->")?
        .trim();
    let rest = inner.strip_prefix("ctx")?;
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    Some(parse_pairs(rest))
}

fn parse_pairs(s: &str) -> Result<Meta, String> {
    let mut meta = Meta::default();
    let mut chars = s.char_indices().peekable();
    loop {
        while chars.next_if(|(_, c)| c.is_whitespace()).is_some() {}
        let Some(&(start, _)) = chars.peek() else {
            break;
        };
        while chars
            .next_if(|(_, c)| *c != '=' && !c.is_whitespace())
            .is_some()
        {}
        let key_end = chars.peek().map_or(s.len(), |(i, _)| *i);
        let key = &s[start..key_end];
        if chars.next_if(|(_, c)| *c == '=').is_none() {
            return Err(format!("{key} 后面缺 `=`"));
        }
        let value = if chars.next_if(|(_, c)| *c == '"').is_some() {
            let mut v = String::new();
            loop {
                match chars.next() {
                    Some((_, '"')) => break,
                    Some((_, '\\')) => match chars.next() {
                        Some((_, c)) => v.push(c),
                        None => return Err(format!("{key} 的值没有结束的引号")),
                    },
                    Some((_, c)) => v.push(c),
                    None => return Err(format!("{key} 的值没有结束的引号")),
                }
            }
            v
        } else {
            let vstart = chars.peek().map_or(s.len(), |(i, _)| *i);
            while chars.next_if(|(_, c)| !c.is_whitespace()).is_some() {}
            let vend = chars.peek().map_or(s.len(), |(i, _)| *i);
            s[vstart..vend].to_owned()
        };
        let slot = match key {
            "id" => &mut meta.id,
            "kind" => &mut meta.kind,
            "tags" => &mut meta.tags,
            "owner" => &mut meta.owner,
            "review_by" => &mut meta.review_by,
            "supersedes" => &mut meta.supersedes,
            other => return Err(format!("不认识的属性 {other}")),
        };
        if slot.replace(value).is_some() {
            return Err(format!("{key} 写了两次"));
        }
    }
    Ok(meta)
}

/// 给文件里没有 id 的条目补上 id。没有要补的返回 `None`。
/// 只插入 id，文件其余部分逐字节不变。
pub fn assign_ids(path: &str, text: &str) -> Option<(String, Vec<(ItemId, String)>)> {
    let parsed = parse_file(path, text);
    let missing: Vec<&ParsedItem> = parsed.items.iter().filter(|i| i.id.is_none()).collect();
    if missing.is_empty() {
        return None;
    }
    let bom = text.starts_with('\u{feff}');
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let eol = if body.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = body.split_inclusive('\n').map(str::to_owned).collect();

    let mut assigned: Vec<(ItemId, String)> = Vec::new();
    // 从下往上改，前面的行号不受影响。
    let mut edits: Vec<(usize, &ParsedItem, ItemId)> = missing
        .iter()
        .map(|item| {
            (
                item.meta_line.unwrap_or(item.insert_at),
                *item,
                ItemId::generate(),
            )
        })
        .collect();
    edits.sort_by_key(|(line, _, _)| std::cmp::Reverse(*line));
    for (_, item, id) in edits {
        match item.meta_line {
            Some(i) => {
                let line = &mut lines[i];
                let at = line.find("<!--").map_or(0, |p| p + 4);
                let ctx = line[at..].find("ctx").map_or(at, |p| at + p + 3);
                line.insert_str(ctx, &format!(" id={id}"));
            }
            None => {
                let at = item.insert_at.min(lines.len());
                if at == lines.len()
                    && let Some(last) = lines.last_mut()
                    && !last.ends_with('\n')
                {
                    last.push_str(eol);
                }
                lines.insert(at, format!("<!-- ctx id={id} -->{eol}"));
            }
        }
        assigned.push((id, item.title.clone()));
    }
    assigned.reverse();
    let mut out = String::with_capacity(text.len() + assigned.len() * 48);
    if bom {
        out.push('\u{feff}');
    }
    out.extend(lines);
    Some((out, assigned))
}

/// 从 Notion 导出的文件和目录名末尾带一个空格加 32 位十六进制，导入时去掉。
pub fn clean_export_name(name: &str) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(p) if p > 0 => (&name[..p], &name[p..]),
        _ => (name, ""),
    };
    let cleaned = match stem.rsplit_once(' ') {
        Some((head, tail))
            if tail.len() == 32
                && tail.bytes().all(|b| b.is_ascii_hexdigit())
                && !head.trim().is_empty() =>
        {
            head.trim_end()
        }
        _ => stem,
    };
    format!("{cleaned}{ext}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID1: &str = "ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA";
    const ID2: &str = "ctx_01JDX4M6Q8BBBBBBBBBBBBBBBB";

    fn ok(path: &str, text: &str) -> Vec<ParsedItem> {
        let parsed = parse_file(path, text);
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        parsed.items
    }

    #[test]
    fn paths() {
        assert_eq!(scope_of_path("org/公司简介.md"), Some(Scope::Org));
        assert_eq!(
            scope_of_path("team-research/a/b.md"),
            Some(Scope::Team("research".into()))
        );
        assert_eq!(scope_of_path("README.md"), None);
        assert_eq!(scope_of_path("org/a.txt"), None);
        assert_eq!(scope_of_path("misc/a.md"), None);
    }

    #[test]
    fn items_by_h2_with_defaults() {
        let text = format!(
            "---\nkind: procedure\ntags: [周报, 客户]\nowner: 研究组\nreview_by: 2027-03-31\n---\n\n# 客户\n\n## 大客户周报用什么格式\n<!-- ctx id={ID1} kind=decision tags=\"格式, 周报\" -->\n\n用表格，日期写全。\n\n### 细节\n\n- 一\n\n## 第二条\n\n正文二\n"
        );
        let items = ok("team-research/客户.md", &text);
        assert_eq!(items.len(), 2);
        let a = &items[0];
        assert_eq!(a.id.as_ref().unwrap().as_str(), ID1);
        assert_eq!(a.kind, Kind::Decision);
        assert_eq!(a.title, "大客户周报用什么格式");
        assert_eq!(a.body, "用表格，日期写全。\n\n### 细节\n\n- 一");
        assert_eq!(a.tags, vec!["格式", "周报"]);
        assert_eq!(a.owner.as_deref(), Some("研究组"));
        assert_eq!(a.review_by.as_deref(), Some("2027-03-31"));
        assert_eq!(a.line, 10);
        let b = &items[1];
        assert_eq!(b.id, None);
        assert_eq!(b.kind, Kind::Procedure);
        assert_eq!(b.tags, vec!["周报", "客户"]);
        assert_eq!(b.body, "正文二");
        assert_eq!(b.scope, Scope::Team("research".into()));
    }

    #[test]
    fn whole_file_is_one_item_without_h2() {
        let text = format!("# 公司简介\n<!-- ctx id={ID1} kind=fact -->\n\n我们做宏观研究。\n");
        let items = ok("org/公司简介.md", &text);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "公司简介");
        assert_eq!(items[0].body, "我们做宏观研究。");

        let items = ok(
            "org/术语.md",
            "---\nkind: term\n---\nCPI 是居民消费价格指数。\n",
        );
        assert_eq!(items[0].title, "术语");
        assert_eq!(items[0].body, "CPI 是居民消费价格指数。");
    }

    #[test]
    fn headings_in_code_blocks_are_not_items() {
        let text = "---\nkind: fact\n---\n## 一\n\n```\n## 不是标题\n```\n\n## 二\nx\n";
        let items = ok("org/a.md", text);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].body, "```\n## 不是标题\n```");
    }

    #[test]
    fn setext_and_closed_atx_headings() {
        let text = "---\nkind: fact\n---\n标题一\n------\n\n正文\n\n## 标题二 ##\n正文二\n";
        let items = ok("org/a.md", text);
        assert_eq!(
            items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
            ["标题一", "标题二"]
        );
        assert_eq!(items[0].body, "正文");
    }

    #[test]
    fn warnings_and_skips() {
        let text = format!(
            "前言\n\n## 没有 kind\n正文\n\n## 坏注释\n<!-- ctx colour=red -->\n\n## 坏 id\n<!-- ctx id=ctx_bad kind=fact -->\n\n## 坏日期\n<!-- ctx kind=fact review_by=2027-02-30 supersedes={ID2} -->\n"
        );
        let parsed = parse_file("org/a.md", &text);
        let titles: Vec<_> = parsed.items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, ["没有 kind", "坏日期"]);
        assert_eq!(parsed.items[0].kind, Kind::Reference);
        assert_eq!(parsed.items[1].review_by, None);
        assert_eq!(parsed.items[1].supersedes.as_ref().unwrap().as_str(), ID2);
        assert_eq!(parsed.warnings.len(), 5, "{:#?}", parsed.warnings);
        assert_eq!(parsed.warnings[0].line, Some(1));
    }

    #[test]
    fn bad_front_matter_skips_file() {
        let parsed = parse_file("org/a.md", "---\ncolour: red\n---\n## x\n");
        assert!(parsed.items.is_empty());
        assert_eq!(parsed.warnings.len(), 1);
    }

    #[test]
    fn content_hash_ignores_path_and_line() {
        let a = &ok("org/a.md", "---\nkind: fact\n---\n## x\ny\n")[0];
        let mut b = ok("org/b.md", "---\nkind: fact\n---\n\n\n## x\n\ny\n\n")[0].clone();
        assert_eq!(a.content_hash(), b.content_hash());
        b.body.push('!');
        assert_ne!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn assign_ids_only_inserts() {
        let text = format!(
            "---\nkind: fact\n---\n## 一\n正文\n\n## 二\n<!-- ctx kind=term -->\n正文二\n\n## 三\n<!-- ctx id={ID1} -->\n"
        );
        let (new, ids) = assign_ids("org/a.md", &text).unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0].1, "一");
        assert_eq!(ids[1].1, "二");
        let expected = format!(
            "---\nkind: fact\n---\n## 一\n<!-- ctx id={} -->\n正文\n\n## 二\n<!-- ctx id={} kind=term -->\n正文二\n\n## 三\n<!-- ctx id={ID1} -->\n",
            ids[0].0, ids[1].0
        );
        assert_eq!(new, expected);
        let reparsed = ok("org/a.md", &new);
        assert!(reparsed.iter().all(|i| i.id.is_some()));
        assert_eq!(reparsed[1].kind, Kind::Term);
        assert!(assign_ids("org/a.md", &new).is_none());
    }

    #[test]
    fn assign_ids_keeps_crlf_bom_and_eof() {
        let text = "\u{feff}---\r\nkind: fact\r\n---\r\n## 一";
        let (new, ids) = assign_ids("org/a.md", text).unwrap();
        assert_eq!(
            new,
            format!(
                "\u{feff}---\r\nkind: fact\r\n---\r\n## 一\r\n<!-- ctx id={} -->\r\n",
                ids[0].0
            )
        );
        let (new, ids) = assign_ids("org/b.md", "没有标题的一条\n").unwrap();
        assert_eq!(
            new,
            format!("<!-- ctx id={} -->\n没有标题的一条\n", ids[0].0)
        );
        let items = parse_file("org/b.md", &new).items;
        assert_eq!(items[0].body, "没有标题的一条");
    }

    #[test]
    fn export_names() {
        assert_eq!(
            clean_export_name("周报格式 1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d.md"),
            "周报格式.md"
        );
        assert_eq!(
            clean_export_name("客户 0123456789abcdef0123456789abcdef"),
            "客户"
        );
        assert_eq!(clean_export_name("普通名字.md"), "普通名字.md");
        assert_eq!(
            clean_export_name("0123456789abcdef0123456789abcdef.md"),
            "0123456789abcdef0123456789abcdef.md"
        );
    }
}
