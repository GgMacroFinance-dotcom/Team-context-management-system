//! 改上下文仓库里的一个文件：按 id 换掉一条、在某条后面插一条、追加一条、删掉一条。
//!
//! 只动那一条占的行（[`ParsedItem::span`]），别的地方逐字节不变。改完以后把整个文件重新解析一遍核对：
//! 别的条目内容不变，写进去的那条读回来和要写的一样。对不上就不改（比如正文里有二级标题，
//! 读回来会多出一条），由调用的人决定怎么办。

use std::collections::BTreeMap;

use crate::format::{FileDefaults, ParsedItem, parse_file};
use crate::model::{ItemId, Kind};

/// 要写进文件的一条。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemText {
    pub id: ItemId,
    pub kind: Kind,
    pub title: String,
    /// Markdown 正文。写之前用 [`normalize_body`] 整理过。
    pub body: String,
    pub tags: Vec<String>,
    pub owner: Option<String>,
    pub review_by: Option<String>,
    pub supersedes: Option<ItemId>,
}

impl ItemText {
    /// 从解析出的一条拿属性（要有 id）。
    pub fn from_parsed(item: &ParsedItem) -> Option<Self> {
        Some(Self {
            id: item.id.clone()?,
            kind: item.kind,
            title: item.title.clone(),
            body: item.body.clone(),
            tags: item.tags.clone(),
            owner: item.owner.clone(),
            review_by: item.review_by.clone(),
            supersedes: item.supersedes.clone(),
        })
    }

    fn matches(&self, p: &ParsedItem) -> bool {
        p.id.as_ref() == Some(&self.id)
            && p.kind == self.kind
            && p.title == self.title
            && p.body == self.body
            && p.tags == self.tags
            && p.owner == self.owner
            && p.review_by == self.review_by
            && p.supersedes == self.supersedes
    }
}

/// 对一个文件做的改动。
#[derive(Debug, Clone)]
pub enum Edit {
    /// 换掉这个 id 的那一条（id 不变）。
    Replace(ItemText),
    /// 插在 `after` 那一条后面。
    InsertAfter { after: ItemId, item: ItemText },
    /// 加在文件末尾；文件不存在时新建。
    Append(ItemText),
    /// 删掉这个 id 的那一条（等于归档）。
    Remove(ItemId),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EditError {
    #[error("文件 {path} 里没有 {id}")]
    NotFound { path: String, id: String },
    #[error("文件 {0} 不存在")]
    NoFile(String),
    #[error("{0} 不是条目文件（第一层目录要是范围，扩展名 .md）")]
    NotItemFile(String),
    #[error(
        "这一条写进 {path} 以后读回来对不上：{why}。正文里不能有一级、二级标题（行首的 # 或 ##，代码块里的不算），标题不能有换行，标签不能有逗号"
    )]
    RoundTrip { path: String, why: String },
}

/// 正文整理成解析器读回来的样子：换行统一成 `\n`，去掉前后的空行，行尾不留 `\r`。
pub fn normalize_body(body: &str) -> String {
    let lines: Vec<&str> = body.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    let first = lines.iter().position(|l| !l.trim().is_empty());
    let last = lines.iter().rposition(|l| !l.trim().is_empty());
    match (first, last) {
        (Some(a), Some(b)) => lines[a..=b].join("\n"),
        _ => String::new(),
    }
}

/// 标题整理成一行：换行换成空格，去掉前后空白。
pub fn normalize_title(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn quote(v: &str) -> String {
    let simple = !v.is_empty()
        && !v
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '\\' | '='))
        && !v.contains("--");
    if simple {
        return v.to_owned();
    }
    let mut s = String::with_capacity(v.len() + 2);
    s.push('"');
    for c in v.chars() {
        if matches!(c, '"' | '\\') {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('"');
    s
}

/// 一条的 Markdown：二级标题、ctx 注释、空行、正文。文件开头有默认值时，没有的属性写成空的盖掉它。
pub fn render(item: &ItemText, defaults: &FileDefaults) -> String {
    let mut meta = format!("<!-- ctx id={} kind={}", item.id, item.kind);
    if !item.tags.is_empty() {
        meta.push_str(&format!(" tags={}", quote(&item.tags.join(", "))));
    } else if defaults.tags.as_ref().is_some_and(|t| !t.is_empty()) {
        meta.push_str(" tags=\"\"");
    }
    match &item.owner {
        Some(o) => meta.push_str(&format!(" owner={}", quote(o))),
        None if defaults.owner.is_some() => meta.push_str(" owner=\"\""),
        None => {}
    }
    match &item.review_by {
        Some(d) => meta.push_str(&format!(" review_by={}", quote(d))),
        None if defaults.review_by.is_some() => meta.push_str(" review_by=\"\""),
        None => {}
    }
    if let Some(s) = &item.supersedes {
        meta.push_str(&format!(" supersedes={s}"));
    }
    meta.push_str(" -->");
    let mut out = format!("## {}\n{meta}\n", item.title);
    if !item.body.is_empty() {
        out.push('\n');
        out.push_str(&item.body);
        out.push('\n');
    }
    out
}

fn blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// 改一个文件。`text` 是现在的内容（文件不存在时为 `None`）。返回改好的内容；`None` 表示这个文件该删掉
/// （删掉最后一条以后只剩文件开头的 YAML、一级标题和空行）。
pub fn apply(path: &str, text: Option<&str>, edit: &Edit) -> Result<Option<String>, EditError> {
    let old_text = text.unwrap_or("");
    if text.is_none() && !matches!(edit, Edit::Append(_)) {
        return Err(EditError::NoFile(path.to_owned()));
    }
    let old = parse_file(path, old_text);
    if crate::format::scope_of_path(path).is_none() {
        return Err(EditError::NotItemFile(path.to_owned()));
    }
    let bom = old_text.starts_with('\u{feff}');
    let body = old_text.strip_prefix('\u{feff}').unwrap_or(old_text);
    let eol = if body.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = body.split_inclusive('\n').map(str::to_owned).collect();
    let block = |item: &ItemText| -> Vec<String> {
        render(item, &old.defaults)
            .split_inclusive('\n')
            .map(|l| l.replace('\n', eol))
            .collect()
    };
    let find = |id: &ItemId| -> Result<&ParsedItem, EditError> {
        old.items
            .iter()
            .find(|it| it.id.as_ref() == Some(id))
            .ok_or_else(|| EditError::NotFound {
                path: path.to_owned(),
                id: id.to_string(),
            })
    };
    // 最后一行没有换行时补上，后面接得上。
    let terminate = |lines: &mut Vec<String>| {
        if let Some(last) = lines.last_mut()
            && !last.ends_with('\n')
        {
            last.push_str(eol);
        }
    };

    let (touched, written): (Option<ItemId>, Option<&ItemText>) = match edit {
        Edit::Replace(item) => {
            let (a, b) = find(&item.id)?.span;
            let mut new = block(item);
            if b < lines.len() {
                new.push(eol.to_owned());
            }
            terminate(&mut lines);
            lines.splice(a..b, new);
            (Some(item.id.clone()), Some(item))
        }
        Edit::InsertAfter { after, item } => {
            let (_, b) = find(after)?.span;
            terminate(&mut lines);
            let mut new = Vec::new();
            if b > 0 && !blank(&lines[b - 1]) {
                new.push(eol.to_owned());
            }
            new.extend(block(item));
            if b < lines.len() {
                new.push(eol.to_owned());
            }
            lines.splice(b..b, new);
            (None, Some(item))
        }
        Edit::Append(item) => {
            terminate(&mut lines);
            if lines.last().is_some_and(|l| !blank(l)) {
                lines.push(eol.to_owned());
            }
            lines.extend(block(item));
            (None, Some(item))
        }
        Edit::Remove(id) => {
            let (a, b) = find(id)?.span;
            lines.drain(a..b);
            // 接口处只留一个空行。
            while a < lines.len() && a > 0 && blank(&lines[a]) && blank(&lines[a - 1]) {
                lines.remove(a);
            }
            while lines.last().is_some_and(|l| blank(l)) {
                lines.pop();
            }
            (Some(id.clone()), None)
        }
    };

    let mut out = String::new();
    if bom {
        out.push('\u{feff}');
    }
    out.extend(lines);
    let new = parse_file(path, &out);

    // 别的条目都还在，内容不变。
    let before: BTreeMap<&ItemId, String> = old
        .items
        .iter()
        .filter_map(|it| Some((it.id.as_ref()?, it.content_hash())))
        .filter(|(id, _)| touched.as_ref() != Some(*id))
        .collect();
    let after: BTreeMap<&ItemId, String> = new
        .items
        .iter()
        .filter_map(|it| Some((it.id.as_ref()?, it.content_hash())))
        .collect();
    let bad = |why: String| EditError::RoundTrip {
        path: path.to_owned(),
        why,
    };
    for (id, hash) in &before {
        if after.get(id) != Some(hash) {
            return Err(bad(format!("别的条目 {id} 变了")));
        }
    }
    match written {
        Some(item) => {
            let found: Vec<&ParsedItem> = new
                .items
                .iter()
                .filter(|it| it.id.as_ref() == Some(&item.id))
                .collect();
            match found.as_slice() {
                [p] if item.matches(p) => {}
                [p] => {
                    return Err(bad(format!(
                        "读回来是“{}”，{} 行正文",
                        p.title,
                        p.body.lines().count()
                    )));
                }
                _ => return Err(bad("读回来找不到这一条".to_owned())),
            }
            let expected = before.len() + 1;
            if new.items.len() != expected {
                return Err(bad(format!(
                    "读回来有 {} 条，应该是 {expected} 条",
                    new.items.len()
                )));
            }
        }
        None => {
            if new.items.len() != before.len() {
                // 删掉最后一条以后，剩下的东西会被当成一整条（没有 id）。只剩 YAML、一级标题、空行时删掉文件。
                let only_frame = new.items.len() == 1
                    && before.is_empty()
                    && new.items[0].body.is_empty()
                    && new.items[0].id.is_none();
                if only_frame || out.trim_start_matches('\u{feff}').trim().is_empty() {
                    return Ok(None);
                }
                return Err(bad(format!(
                    "删掉以后剩下的内容读出 {} 条，应该是 {} 条",
                    new.items.len(),
                    before.len()
                )));
            }
            if before.is_empty() && !has_content(&out, new.body_start) {
                return Ok(None);
            }
        }
    }
    Ok(Some(out))
}

/// YAML 之后除了一级标题和空行还有没有别的。
fn has_content(text: &str, body_start: usize) -> bool {
    text.trim_start_matches('\u{feff}')
        .split_inclusive('\n')
        .skip(body_start)
        .any(|l| {
            let t = l.trim();
            !t.is_empty() && !(t.starts_with("# ") || t == "#")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA";
    const B: &str = "ctx_01JDX4M6Q8BBBBBBBBBBBBBBBB";
    const C: &str = "ctx_01JDX4M6Q8CCCCCCCCCCCCCCCC";

    fn id(s: &str) -> ItemId {
        s.parse().unwrap()
    }

    fn text(i: &str, title: &str, body: &str) -> ItemText {
        ItemText {
            id: id(i),
            kind: Kind::Fact,
            title: title.into(),
            body: body.into(),
            tags: vec![],
            owner: None,
            review_by: None,
            supersedes: None,
        }
    }

    const FILE: &str = "# 数据源\n\n## 第一条\n<!-- ctx id=ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA kind=fact -->\n\n一。\n\n## 第二条\n<!-- ctx id=ctx_01JDX4M6Q8BBBBBBBBBBBBBBBB kind=decision tags=\"周报, 口径\" -->\n\n二。\n";

    #[test]
    fn replace_keeps_the_rest_byte_for_byte() {
        let new = apply(
            "role-macro/数据源.md",
            Some(FILE),
            &Edit::Replace(text(A, "第一条改了", "一改了。\n\n多一段。")),
        )
        .unwrap()
        .unwrap();
        assert!(new.starts_with("# 数据源\n\n## 第一条改了\n<!-- ctx id=ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA kind=fact -->\n\n一改了。\n\n多一段。\n\n## 第二条\n"));
        assert!(new.ends_with(&FILE[FILE.find("## 第二条").unwrap()..]));
        // 换最后一条。
        let mut b = text(B, "第二条", "二改了。");
        b.kind = Kind::Decision;
        b.tags = vec!["周报".into()];
        let new = apply("role-macro/数据源.md", Some(FILE), &Edit::Replace(b))
            .unwrap()
            .unwrap();
        assert!(new.starts_with(&FILE[..FILE.find("## 第二条").unwrap()]));
        assert!(new.ends_with("## 第二条\n<!-- ctx id=ctx_01JDX4M6Q8BBBBBBBBBBBBBBBB kind=decision tags=周报 -->\n\n二改了。\n"));
    }

    #[test]
    fn insert_append_remove() {
        let mut c = text(C, "新的", "取代第一条。");
        c.supersedes = Some(id(A));
        let new = apply(
            "role-macro/数据源.md",
            Some(FILE),
            &Edit::InsertAfter {
                after: id(A),
                item: c.clone(),
            },
        )
        .unwrap()
        .unwrap();
        let p = parse_file("role-macro/数据源.md", &new);
        let ids: Vec<String> = p
            .items
            .iter()
            .map(|i| i.id.clone().unwrap().to_string())
            .collect();
        assert_eq!(ids, [A, C, B]);
        assert!(new.contains(
            "supersedes=ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA -->\n\n取代第一条。\n\n## 第二条"
        ));

        let new = apply("role-macro/数据源.md", Some(FILE), &Edit::Append(c.clone()))
            .unwrap()
            .unwrap();
        assert!(new.starts_with(FILE) && new.ends_with("\n\n## 新的\n<!-- ctx id=ctx_01JDX4M6Q8CCCCCCCCCCCCCCCC kind=fact supersedes=ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA -->\n\n取代第一条。\n"));
        // 新文件。
        let new = apply(
            "role-macro/记忆.md",
            None,
            &Edit::Append(text(C, "新的", "")),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            new,
            "## 新的\n<!-- ctx id=ctx_01JDX4M6Q8CCCCCCCCCCCCCCCC kind=fact -->\n"
        );

        let new = apply("role-macro/数据源.md", Some(FILE), &Edit::Remove(id(A)))
            .unwrap()
            .unwrap();
        assert_eq!(
            new,
            "# 数据源\n\n## 第二条\n<!-- ctx id=ctx_01JDX4M6Q8BBBBBBBBBBBBBBBB kind=decision tags=\"周报, 口径\" -->\n\n二。\n"
        );
        let new = apply("role-macro/数据源.md", Some(&new), &Edit::Remove(id(B))).unwrap();
        assert_eq!(new, None, "只剩一级标题，删掉文件");
        // 单条文件。
        let single = "---\nkind: fact\n---\n<!-- ctx id=ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA -->\n整个文件一条。\n";
        assert_eq!(
            apply("org/简介.md", Some(single), &Edit::Remove(id(A))).unwrap(),
            None
        );
        // 往单条文件里加：原来那条会变，不让加。
        assert!(matches!(
            apply(
                "org/简介.md",
                Some(single),
                &Edit::Append(text(C, "x", "y"))
            ),
            Err(EditError::RoundTrip { .. })
        ));
    }

    #[test]
    fn refuses_what_does_not_read_back() {
        for (title, body) in [
            ("标题", "正文\n\n## 里面有二级标题\n"),
            ("标题", "# 一级标题"),
            ("", "空标题"),
        ] {
            assert!(
                matches!(
                    apply("org/a.md", Some(FILE), &Edit::Replace(text(A, title, body))),
                    Err(EditError::RoundTrip { .. })
                ),
                "{title} {body}"
            );
        }
        // 代码块里的 ## 可以。
        let body = "```\n## 不是标题\n```";
        assert!(
            apply(
                "org/a.md",
                Some(FILE),
                &Edit::Replace(text(A, "标题", body))
            )
            .is_ok()
        );
        let mut t = text(A, "标题", "x");
        t.tags = vec!["a,b".into()];
        assert!(apply("org/a.md", Some(FILE), &Edit::Replace(t)).is_err());
        assert_eq!(
            apply("org/a.md", Some(FILE), &Edit::Remove(id(C))),
            Err(EditError::NotFound {
                path: "org/a.md".into(),
                id: C.into()
            })
        );
    }

    #[test]
    fn defaults_are_overridden_and_quotes_escaped() {
        let file = "---\ntags: [周报]\nowner: 研究部\nreview_by: 2027-01-01\n---\n## 一条\n<!-- ctx id=ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA kind=fact -->\n正文\n";
        let mut t = text(A, "一条", "正文");
        t.owner = Some("张 \"三\"".into());
        let new = apply("org/a.md", Some(file), &Edit::Replace(t.clone()))
            .unwrap()
            .unwrap();
        assert!(
            new.contains(r#"tags="" owner="张 \"三\"" review_by="" -->"#),
            "{new}"
        );
        let p = parse_file("org/a.md", &new);
        assert!(t.matches(&p.items[0]));
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    }

    #[test]
    fn crlf_and_bom_are_kept() {
        let file = "\u{feff}## 一条\r\n<!-- ctx id=ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA kind=fact -->\r\n正文\r\n";
        let new = apply("org/a.md", Some(file), &Edit::Append(text(B, "二", "二。")))
            .unwrap()
            .unwrap();
        assert!(new.starts_with(file));
        assert!(new.ends_with("\r\n\r\n## 二\r\n<!-- ctx id=ctx_01JDX4M6Q8BBBBBBBBBBBBBBBB kind=fact -->\r\n\r\n二。\r\n"));
        assert_eq!(normalize_body("\r\n\r\n一\r\n二\r\n\r\n"), "一\n二");
        assert_eq!(normalize_title("  多行\n标题 "), "多行 标题");
    }
}
