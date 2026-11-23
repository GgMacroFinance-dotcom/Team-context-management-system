//! 给审核的人看的：一条提议和现在的条目差在哪里（命令行 `tcms proposals <id>` 和第 5 步的审核页共用）。
//!
//! 比的是条目“读起来”的样子：标题、类型、标签、复核日期各一行，空一行，接着正文。按行比。

use serde::Serialize;
use similar::{ChangeTag, TextDiff};
use tcms_store::{ItemRow, ProposalRow};

/// 一行差异。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "op", content = "line", rename_all = "lowercase")]
pub enum DiffLine {
    Same(String),
    Removed(String),
    Added(String),
}

/// 条目读起来的样子。
pub fn item_text(
    title: &str,
    kind: &str,
    tags: &[String],
    review_by: Option<&str>,
    body: &str,
) -> String {
    let mut s = format!("标题：{title}\n类型：{kind}\n");
    if !tags.is_empty() {
        s.push_str(&format!("标签：{}\n", tags.join("、")));
    }
    if let Some(d) = review_by {
        s.push_str(&format!("复核：{d}\n"));
    }
    s.push('\n');
    s.push_str(body);
    if !body.ends_with('\n') {
        s.push('\n');
    }
    s
}

fn row_text(r: &ItemRow) -> String {
    item_text(&r.title, &r.kind, &r.tags, r.review_by.as_deref(), &r.body)
}

/// 两段文字按行比。
pub fn diff(old: &str, new: &str) -> Vec<DiffLine> {
    TextDiff::from_lines(old, new)
        .iter_all_changes()
        .map(|c| {
            let line = c.value().trim_end_matches('\n').to_owned();
            match c.tag() {
                ChangeTag::Equal => DiffLine::Same(line),
                ChangeTag::Delete => DiffLine::Removed(line),
                ChangeTag::Insert => DiffLine::Added(line),
            }
        })
        .collect()
}

/// 批准以后会变成什么样：`current` 是要改、要取代、要归档的那一条现在的样子（新建时没有）。
/// 新建全是加的；归档全是删的；修改、取代和现在的那一条比。
pub fn proposal_diff(p: &ProposalRow, current: Option<&ItemRow>) -> Vec<DiffLine> {
    let old = current.map(row_text).unwrap_or_default();
    let new = if p.action == "archive" {
        String::new()
    } else {
        item_text(&p.title, &p.kind, &p.tags, p.review_by.as_deref(), &p.body)
    };
    diff(&old, &new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_diff() {
        let old = item_text("CPI", "fact", &["数据".into()], None, "第一行\n第二行\n");
        let new = item_text(
            "CPI",
            "fact",
            &["数据".into()],
            Some("2027-01-01"),
            "第一行\n改过的第二行\n",
        );
        let d = diff(&old, &new);
        assert_eq!(
            d,
            [
                DiffLine::Same("标题：CPI".into()),
                DiffLine::Same("类型：fact".into()),
                DiffLine::Same("标签：数据".into()),
                DiffLine::Added("复核：2027-01-01".into()),
                DiffLine::Same(String::new()),
                DiffLine::Same("第一行".into()),
                DiffLine::Removed("第二行".into()),
                DiffLine::Added("改过的第二行".into()),
            ]
        );
        assert_eq!(
            serde_json::to_value(&d[3]).unwrap(),
            serde_json::json!({"op": "added", "line": "复核：2027-01-01"})
        );
        assert!(
            diff("", "a\n")
                .iter()
                .all(|l| matches!(l, DiffLine::Added(_)))
        );
    }
}
