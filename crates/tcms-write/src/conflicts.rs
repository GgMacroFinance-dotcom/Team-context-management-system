//! 找矛盾（`docs/spec.md` 第 5 节）：同范围、同类型、标题或标签相近的条目，列给审核的人对照着看。
//!
//! 标题按检索一样的规则切词（中文相邻两字），两边的词重合的比例 = 共有的词数 / 词少的那边的词数；
//! 标签（不算 `pinned`）按交集 / 并集。两个取大的，到一半就算相近。确定性的，同样的条目结果一样。

use std::collections::BTreeSet;

use serde::Serialize;
use tcms_core::{Kind, Scope};
use tcms_search::{Corpus, MAX_QUERY_TERMS, distinct_terms};

/// 最多列几条。
const MAX_CONFLICTS: usize = 5;
/// 相近的下限（百分比）。
const MIN_SIMILARITY: u32 = 50;

/// 一条相近的条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Conflict {
    pub id: String,
    pub revision: i32,
    pub scope: String,
    pub kind: String,
    pub title: String,
    /// 相近程度，0 到 100。
    pub similarity: u32,
}

pub(crate) fn terms(text: &str) -> BTreeSet<String> {
    distinct_terms(text, MAX_QUERY_TERMS).into_iter().collect()
}

pub(crate) fn tag_set(tags: &[String]) -> BTreeSet<&str> {
    tags.iter()
        .map(String::as_str)
        .filter(|t| *t != "pinned")
        .collect()
}

/// 两条有多相近（0 到 100）：标题的词重合的比例（共有的词数 / 词少的那边的词数），
/// 和标签的交集 / 并集，取大的。
pub(crate) fn similarity(
    a_terms: &BTreeSet<String>,
    a_tags: &BTreeSet<&str>,
    b_terms: &BTreeSet<String>,
    b_tags: &BTreeSet<&str>,
) -> u32 {
    score(
        a_terms.intersection(b_terms).count(),
        a_terms.len(),
        b_terms.len(),
        a_tags.intersection(b_tags).count(),
        a_tags.len(),
        b_tags.len(),
    )
}

/// [`similarity`] 的算法，用已经数好的共有词数、共有标签数算。
pub(crate) fn score(
    shared_terms: usize,
    a_terms: usize,
    b_terms: usize,
    shared_tags: usize,
    a_tags: usize,
    b_tags: usize,
) -> u32 {
    let title_sim = (shared_terms * 100)
        .checked_div(a_terms.min(b_terms))
        .unwrap_or(0) as u32;
    let tag_sim = (shared_tags * 100)
        .checked_div(a_tags + b_tags - shared_tags)
        .unwrap_or(0) as u32;
    title_sim.max(tag_sim)
}

/// 到这个程度就算相近。
pub(crate) fn similar(similarity: u32) -> bool {
    similarity >= MIN_SIMILARITY
}

/// `exclude`：要改、要取代的那一条自己不算。
pub fn find_conflicts(
    corpus: &Corpus,
    scope: &Scope,
    kind: Kind,
    title: &str,
    tags: &[String],
    exclude: Option<&str>,
) -> Vec<Conflict> {
    let mine = terms(title);
    let my_tags = tag_set(tags);
    let mut found: Vec<Conflict> = corpus
        .items()
        .iter()
        .filter(|it| &it.scope == scope && it.kind == kind && Some(it.id.as_str()) != exclude)
        .filter_map(|it| {
            let similarity = similarity(&mine, &my_tags, &terms(&it.title), &tag_set(&it.tags));
            similar(similarity).then(|| Conflict {
                id: it.id.clone(),
                revision: it.revision,
                scope: it.scope.to_string(),
                kind: it.kind.to_string(),
                title: it.title.clone(),
                similarity,
            })
        })
        .collect();
    found.sort_by(|a, b| {
        b.similarity
            .cmp(&a.similarity)
            .then_with(|| a.id.cmp(&b.id))
    });
    found.truncate(MAX_CONFLICTS);
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use tcms_search::CorpusItem;

    fn item(id: &str, scope: Scope, kind: Kind, title: &str, tags: &[&str]) -> CorpusItem {
        CorpusItem {
            id: id.into(),
            scope,
            kind,
            authority: kind.authority(),
            title: title.into(),
            body: String::new(),
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            owner: None,
            review_by: None,
            supersedes: None,
            revision: 2,
            path: "x.md".into(),
            updated_ms: 0,
        }
    }

    #[test]
    fn same_scope_same_kind_similar_title_or_tags() {
        let role = Scope::Role("macro".into());
        let corpus = Corpus::new(
            vec![
                item("ctx_a", role.clone(), Kind::Fact, "CPI 数据从哪里取", &[]),
                item("ctx_b", role.clone(), Kind::Fact, "完全无关的事", &["口径"]),
                item(
                    "ctx_c",
                    role.clone(),
                    Kind::Decision,
                    "CPI 数据从哪里取",
                    &[],
                ),
                item("ctx_d", Scope::Org, Kind::Fact, "CPI 数据从哪里取", &[]),
                item("ctx_e", role.clone(), Kind::Fact, "PPI 的口径", &["pinned"]),
            ],
            1,
            None,
        );
        let found = find_conflicts(
            &corpus,
            &role,
            Kind::Fact,
            "CPI 数据该从哪取",
            &["口径".into()],
            None,
        );
        let ids: Vec<&str> = found.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["ctx_b", "ctx_a"], "{found:?}");
        assert_eq!(found[0].similarity, 100);
        // 自己不算。
        let found = find_conflicts(
            &corpus,
            &role,
            Kind::Fact,
            "CPI 数据从哪里取",
            &[],
            Some("ctx_a"),
        );
        assert!(found.is_empty());
    }
}
