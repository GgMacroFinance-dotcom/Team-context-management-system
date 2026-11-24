//! 系统找晋升候选（`docs/spec.md` 第 5 节“晋升”）：同一类型、标题或标签相近，却分别写在几个岗位里的条目，
//! 建议提到它们共同的团队（几个岗位在 `teams.yaml` 的同一个团队里时）或全公司；几个团队、项目都写了的，
//! 建议提到全公司。只是建议：人挑一条发起晋升（[`crate::promote`]），做成提议，再批准一次才生效。
//!
//! 相近的算法和找矛盾一样（`conflicts`）。两两比太慢（1 万条约 5 千万对），先按标题的词和标签建倒排，
//! 只比至少有一个词或标签相同的。相近的连成一组（并查集）。要提到的范围里已经有相近的条目时不再建议。
//! 确定性的：同样的条目，结果一样。
//!
//! 另一种候选是用得多的（[`used_candidates`]）：岗位、团队、项目里的一条，最近 [`USE_WINDOW_DAYS`] 天被 agent
//! 用了很多次（`context_get` 按任务挑中、`context_read` 读了，记在 `item_uses` 表里），建议人看看要不要提上去。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::Serialize;
use tcms_core::{Scope, Teams};
use tcms_search::Corpus;
use tcms_store::ItemUse;

use crate::conflicts::{find_conflicts, score, similar, tag_set, terms};

/// 用得多的看最近几天。
pub const USE_WINDOW_DAYS: i64 = 30;
/// 用得多：最近 [`USE_WINDOW_DAYS`] 天里至少用了这么多次……
pub const MIN_USES: i64 = 20;
/// ……并且在至少这么多天里用过（一天里集中用很多次的不算）。
pub const MIN_USE_DAYS: i64 = 5;

/// 一组建议提到更大范围的条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromotionCandidate {
    /// 建议提到哪个范围：`team:<团队>` 或 `org`。
    pub to: String,
    pub kind: String,
    /// 相近的几条，按范围、id 排，至少在两个范围里。
    pub items: Vec<CandidateItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CandidateItem {
    pub id: String,
    pub scope: String,
    pub title: String,
    pub revision: i32,
}

/// 哪些条目放在一起比：岗位和岗位比；团队、项目和团队、项目比（都只能提到全公司）。全公司的不比。
fn level(scope: &Scope) -> Option<u8> {
    match scope {
        Scope::Role(_) => Some(0),
        Scope::Team(_) | Scope::Project(_) => Some(1),
        Scope::Org => None,
    }
}

fn root(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

/// 所有的晋升候选，几个范围都写了的多的在前。`teams` 是上下文仓库里的 `teams.yaml`。
pub fn promotion_candidates(corpus: &Corpus, teams: Option<&Teams>) -> Vec<PromotionCandidate> {
    let items = corpus.items();
    let title_terms: Vec<BTreeSet<String>> = items.iter().map(|it| terms(&it.title)).collect();
    let tags: Vec<BTreeSet<&str>> = items.iter().map(|it| tag_set(&it.tags)).collect();
    // 倒排：（类型，是不是标签，词或标签）→ 条目的下标，从小到大。
    let mut postings: HashMap<(&str, bool, &str), Vec<usize>> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        let kind = it.kind.as_str();
        for t in &title_terms[i] {
            postings.entry((kind, false, t)).or_default().push(i);
        }
        for g in &tags[i] {
            postings.entry((kind, true, g)).or_default().push(i);
        }
    }
    // 同一层级、不同范围、相近的连成一组；顺便记下每条在全公司、团队里相近的条目（看要提到的范围里有没有）。
    // 和第 i 条类型一样、至少有一个词或标签相同的，边走倒排边数共有几个词、几个标签，不再逐对求交集。
    let n = items.len();
    let mut parent: Vec<usize> = (0..n).collect();
    let mut near: Vec<Vec<usize>> = vec![Vec::new(); n];
    let (mut term_hits, mut tag_hits) = (vec![0usize; n], vec![0usize; n]);
    let mut touched: Vec<usize> = Vec::new();
    for i in 0..n {
        let Some(li) = level(&items[i].scope) else {
            continue;
        };
        let kind = items[i].kind.as_str();
        touched.clear();
        for t in &title_terms[i] {
            for &j in postings
                .get(&(kind, false, t.as_str()))
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                if term_hits[j] == 0 && tag_hits[j] == 0 {
                    touched.push(j);
                }
                term_hits[j] += 1;
            }
        }
        for g in &tags[i] {
            for &j in postings
                .get(&(kind, true, *g))
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                if term_hits[j] == 0 && tag_hits[j] == 0 {
                    touched.push(j);
                }
                tag_hits[j] += 1;
            }
        }
        touched.sort_unstable();
        for &j in &touched {
            let (shared_terms, shared_tags) = (term_hits[j], tag_hits[j]);
            term_hits[j] = 0;
            tag_hits[j] = 0;
            if j == i {
                continue;
            }
            let same_level = level(&items[j].scope) == Some(li) && items[j].scope != items[i].scope;
            let wider = matches!(items[j].scope, Scope::Org | Scope::Team(_));
            if !same_level && !wider {
                continue;
            }
            let s = score(
                shared_terms,
                title_terms[i].len(),
                title_terms[j].len(),
                shared_tags,
                tags[i].len(),
                tags[j].len(),
            );
            if !similar(s) {
                continue;
            }
            if wider {
                near[i].push(j);
            }
            if same_level && j > i {
                let (a, b) = (root(&mut parent, i), root(&mut parent, j));
                if a != b {
                    parent[a.max(b)] = a.min(b);
                }
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (i, it) in items.iter().enumerate() {
        if level(&it.scope).is_some() {
            let r = root(&mut parent, i);
            groups.entry(r).or_default().push(i);
        }
    }

    let mut out: Vec<(usize, PromotionCandidate)> = Vec::new();
    for members in groups.into_values() {
        let scopes: BTreeSet<&Scope> = members.iter().map(|&i| &items[i].scope).collect();
        if scopes.len() < 2 {
            continue;
        }
        let to = target(&scopes, teams);
        // 要提到的范围里已经有相近的条目：不再建议（要收掉重复的，人去归档）。
        if members
            .iter()
            .any(|&i| near[i].iter().any(|&j| items[j].scope == to))
        {
            continue;
        }
        let mut list: Vec<CandidateItem> = members
            .iter()
            .map(|&i| CandidateItem {
                id: items[i].id.clone(),
                scope: items[i].scope.to_string(),
                title: items[i].title.clone(),
                revision: items[i].revision,
            })
            .collect();
        list.sort_by(|a, b| a.scope.cmp(&b.scope).then_with(|| a.id.cmp(&b.id)));
        out.push((
            scopes.len(),
            PromotionCandidate {
                to: to.to_string(),
                kind: items[members[0]].kind.to_string(),
                items: list,
            },
        ));
    }
    out.sort_by(|(a_n, a), (b_n, b)| {
        b_n.cmp(a_n)
            .then_with(|| b.items.len().cmp(&a.items.len()))
            .then_with(|| a.items[0].id.cmp(&b.items[0].id))
    });
    out.into_iter().map(|(_, c)| c).collect()
}

/// 一条用得多、建议提到更大范围的条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsedCandidate {
    /// 建议提到哪个范围：岗位在 `teams.yaml` 的团队里时是 `team:<团队>`，别的是 `org`。
    pub to: String,
    pub kind: String,
    pub item: CandidateItem,
    /// 最近 [`USE_WINDOW_DAYS`] 天用了几次、在几天里用过、哪些岗位用过。
    pub uses: i64,
    pub days: i64,
    pub roles: Vec<String>,
}

/// 用得多的晋升候选，用得多的在前。`uses` 是最近 [`USE_WINDOW_DAYS`] 天每条用了多少次
/// （`Store::recent_uses`），`skip` 里的（已经在 [`promotion_candidates`] 的组里的）不再列。
/// 全公司的、已经不是生效的、要提到的范围里已经有相近条目的不列。
pub fn used_candidates(
    corpus: &Corpus,
    teams: Option<&Teams>,
    uses: &[ItemUse],
    skip: &HashSet<&str>,
) -> Vec<UsedCandidate> {
    let mut out: Vec<UsedCandidate> = uses
        .iter()
        .filter(|u| u.uses >= MIN_USES && u.days >= MIN_USE_DAYS)
        .filter(|u| !skip.contains(u.item_id.as_str()))
        .filter_map(|u| {
            let it = corpus.get(&u.item_id)?;
            level(&it.scope)?;
            let to = target(&BTreeSet::from([&it.scope]), teams);
            if !find_conflicts(corpus, &to, it.kind, &it.title, &it.tags, Some(&it.id)).is_empty() {
                return None;
            }
            Some(UsedCandidate {
                to: to.to_string(),
                kind: it.kind.to_string(),
                item: CandidateItem {
                    id: it.id.clone(),
                    scope: it.scope.to_string(),
                    title: it.title.clone(),
                    revision: it.revision,
                },
                uses: u.uses,
                days: u.days,
                roles: u.roles.clone(),
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.uses
            .cmp(&a.uses)
            .then_with(|| b.days.cmp(&a.days))
            .then_with(|| a.item.id.cmp(&b.item.id))
    });
    out
}

/// 几个岗位都在 `teams.yaml` 的同一个团队里时提到那个团队，别的都提到全公司。
fn target(scopes: &BTreeSet<&Scope>, teams: Option<&Teams>) -> Scope {
    let mut team = None;
    for scope in scopes {
        let Scope::Role(role) = scope else {
            return Scope::Org;
        };
        match teams.and_then(|t| t.team_of_role(role)) {
            Some(t) if team.is_none_or(|x| x == t) => team = Some(t),
            _ => return Scope::Org,
        }
    }
    team.map_or(Scope::Org, |t| Scope::Team(t.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tcms_core::Kind;
    use tcms_search::CorpusItem;

    fn item(id: &str, scope: &str, kind: Kind, title: &str, tags: &[&str]) -> CorpusItem {
        CorpusItem {
            id: id.into(),
            scope: scope.parse().unwrap(),
            kind,
            authority: kind.authority(),
            title: title.into(),
            body: String::new(),
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            owner: None,
            review_by: None,
            supersedes: None,
            revision: 1,
            path: "x.md".into(),
            updated_ms: 0,
        }
    }

    fn found(c: &[PromotionCandidate]) -> Vec<(String, Vec<String>)> {
        c.iter()
            .map(|c| (c.to.clone(), c.items.iter().map(|i| i.id.clone()).collect()))
            .collect()
    }

    #[test]
    fn groups_similar_items_across_scopes() {
        let teams = Teams::parse(
            "teams:\n  research:\n    roles: [macro, data]\n  ops:\n    roles: [legal]\n",
        )
        .unwrap();
        let corpus = Corpus::new(
            vec![
                // 研究团队的两个岗位都写了：建议提到团队。
                item("ctx_a1", "role:macro", Kind::Fact, "CPI 数据从哪里取", &[]),
                item("ctx_a2", "role:data", Kind::Fact, "CPI 数据该从哪取", &[]),
                // 类型不一样，不算。
                item(
                    "ctx_a3",
                    "role:data",
                    Kind::Decision,
                    "CPI 数据从哪里取",
                    &[],
                ),
                // 两个团队的岗位都写了：建议提到全公司。标签一样也算相近。
                item(
                    "ctx_b1",
                    "role:macro",
                    Kind::Procedure,
                    "周报怎么排版",
                    &["周报"],
                ),
                item(
                    "ctx_b2",
                    "role:legal",
                    Kind::Procedure,
                    "合同模板",
                    &["周报"],
                ),
                // 只有一个岗位写了：不建议。
                item("ctx_c1", "role:macro", Kind::Memory, "PMI 看新订单", &[]),
                item(
                    "ctx_c2",
                    "role:macro",
                    Kind::Memory,
                    "PMI 看新订单分项",
                    &[],
                ),
                // 团队、项目都写了：建议提到全公司。
                item("ctx_d1", "team:research", Kind::Rule, "对外数字要复核", &[]),
                item(
                    "ctx_d2",
                    "project:q4",
                    Kind::Rule,
                    "对外数字要复核两遍",
                    &[],
                ),
                // 团队里已经有相近的：不再建议。
                item("ctx_e1", "role:macro", Kind::Fact, "PPI 的口径", &[]),
                item("ctx_e2", "role:data", Kind::Fact, "PPI 的口径说明", &[]),
                item("ctx_e3", "team:research", Kind::Fact, "PPI 口径", &[]),
                // 岗位不在 teams.yaml 里：只能提到全公司。
                item(
                    "ctx_f1",
                    "role:macro",
                    Kind::Decision,
                    "季报只用季度数",
                    &[],
                ),
                item(
                    "ctx_f2",
                    "role:intern",
                    Kind::Decision,
                    "季报只用季度数据",
                    &[],
                ),
            ],
            1,
            None,
        );
        let c = promotion_candidates(&corpus, Some(&teams));
        let mut got = found(&c);
        got.sort();
        let want: Vec<(String, Vec<String>)> = vec![
            ("org".into(), vec!["ctx_b2".into(), "ctx_b1".into()]),
            ("org".into(), vec!["ctx_d2".into(), "ctx_d1".into()]),
            ("org".into(), vec!["ctx_f2".into(), "ctx_f1".into()]),
            (
                "team:research".into(),
                vec!["ctx_a2".into(), "ctx_a1".into()],
            ),
        ];
        assert_eq!(got, want, "{c:?}");
        assert!(c.iter().any(|c| c.kind == "rule"));
        // 没有 teams.yaml：岗位的都提到全公司。
        let c = promotion_candidates(&corpus, None);
        assert!(c.iter().all(|c| c.to == "org"));
        assert!(
            found(&c)
                .iter()
                .any(|(_, ids)| ids.contains(&"ctx_a1".to_owned()))
        );
        // 同样的条目，结果一样。
        assert_eq!(
            promotion_candidates(&corpus, Some(&teams)),
            promotion_candidates(&corpus, Some(&teams))
        );
    }

    #[test]
    fn heavily_used_items() {
        let teams = Teams::parse("teams:\n  research:\n    roles: [macro, data]\n").unwrap();
        let corpus = Corpus::new(
            vec![
                item("ctx_u1", "role:macro", Kind::Fact, "CPI 数据从哪里取", &[]),
                item("ctx_u2", "role:intern", Kind::Fact, "实习生手册", &[]),
                item(
                    "ctx_u3",
                    "team:research",
                    Kind::Procedure,
                    "周报怎么排版",
                    &[],
                ),
                item("ctx_u4", "org", Kind::Fact, "公司地址", &[]),
                item("ctx_u5", "role:macro", Kind::Fact, "PPI 的口径", &[]),
                item("ctx_u6", "team:research", Kind::Fact, "PPI 口径", &[]),
                item("ctx_u7", "role:macro", Kind::Memory, "很少用的", &[]),
                item(
                    "ctx_u8",
                    "role:macro",
                    Kind::Memory,
                    "一天里用了很多次",
                    &[],
                ),
            ],
            1,
            None,
        );
        let used = |id: &str, uses: i64, days: i64| ItemUse {
            item_id: id.into(),
            uses,
            days,
            roles: vec!["macro".into()],
        };
        let uses = vec![
            used("ctx_u1", 40, 10),
            used("ctx_u2", 25, 6),
            used("ctx_u3", 30, 12),
            // 全公司的、要提到的范围里已经有相近的、用得少的、只在一两天里用的、已经不在的：不列。
            used("ctx_u4", 99, 30),
            used("ctx_u5", 50, 20),
            used("ctx_u7", 5, 5),
            used("ctx_u8", 60, 2),
            used("ctx_gone", 80, 20),
        ];
        let c = used_candidates(&corpus, Some(&teams), &uses, &HashSet::new());
        let got: Vec<(&str, &str, i64)> = c
            .iter()
            .map(|c| (c.item.id.as_str(), c.to.as_str(), c.uses))
            .collect();
        assert_eq!(
            got,
            [
                ("ctx_u1", "team:research", 40),
                ("ctx_u3", "org", 30),
                ("ctx_u2", "org", 25)
            ]
        );
        // 已经在相近的那几组里的不再列。
        let c = used_candidates(&corpus, Some(&teams), &uses, &HashSet::from(["ctx_u1"]));
        assert_eq!(c[0].item.id, "ctx_u3");
    }

    #[test]
    fn three_roles_chain_into_one_group() {
        let teams = Teams::parse("teams:\n  research:\n    roles: [a, b, c]\n").unwrap();
        let corpus = Corpus::new(
            vec![
                item("ctx_1", "role:a", Kind::Fact, "统计局的数据怎么取", &[]),
                item("ctx_2", "role:b", Kind::Fact, "统计局的数据", &[]),
                item("ctx_3", "role:c", Kind::Fact, "统计局数据", &["x"]),
            ],
            1,
            None,
        );
        let c = promotion_candidates(&corpus, Some(&teams));
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].to, "team:research");
        assert_eq!(c[0].items.len(), 3);
        assert_eq!(c[0].items[0].scope, "role:a");
    }
}
