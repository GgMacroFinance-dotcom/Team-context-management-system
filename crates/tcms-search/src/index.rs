//! tantivy 内存索引。
//!
//! 一份 [`Corpus`] 建一个索引：在调用的线程里按 id 顺序直接写成一个段（tantivy 的
//! `single_segment_index_writer`，不起后台线程）。BM25 的统计量（文档数、词频、字段长度）只由条目决定，
//! 所以同样的条目分数一样；一个段也是为了以后增量更新时不把删掉的文档算进统计量
//! （tantivy 在合并前还把它们算在里面），查询时也少走几个段。分数量化到千分之一后再排序，
//! 同分按地位、更新时间、id 排，结果逐字节可重现。
//!
//! 不起后台线程是为了内存：tantivy 的写入线程每次新建，分配落在不同的线程上，常驻服务换几次索引
//! 物理内存就涨一截（`tcms bench serve` 量过）。这个接口在 tantivy 里标了 `doc(hidden)`，
//! 升级 tantivy 时要看它还在不在。
//! 索引不落盘：1 万条在内存里建一次只要几百毫秒（`tcms bench read`），从 PostgreSQL 随时能重建。

use std::cmp::Reverse;
use std::collections::HashSet;

use serde::Serialize;
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::Column;
use tantivy::directory::RamDirectory;
use tantivy::merge_policy::NoMergePolicy;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::{FAST, Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions};
use tantivy::tokenizer::{TextAnalyzer, TokenizerManager};
use tantivy::{
    DocId, Index, IndexReader, IndexWriter, ReloadPolicy, Score, SegmentOrdinal, SegmentReader,
    TantivyDocument, Term,
};
use tcms_core::Kind;

use crate::Error;
use crate::corpus::{Corpus, CorpusItem, Visibility};
use crate::tokenize::{TOKENIZER, TcmsTokenizer, distinct_terms, tokenize};

/// 查询最多取多少个不重复的词（按出现顺序）。任务描述很长时只看前面这些。
pub const MAX_QUERY_TERMS: usize = 256;
/// 各字段的权重：标题、标签比正文重。
const TITLE_BOOST: Score = 3.0;
const TAGS_BOOST: Score = 2.0;
/// 建索引时给 tantivy 的内存。
const WRITER_MEMORY: usize = 64 << 20;
/// 摘录的长度（字）和命中词前面留的字数。
const EXCERPT_CHARS: usize = 120;
const EXCERPT_LEAD: usize = 30;
/// `context_search` 最多返回多少条。
pub const MAX_LIMIT: usize = 50;

/// 一份条目和它的检索索引。条目在 `corpus` 里的位置就是索引里的 `ord`。
pub struct SearchIndex {
    corpus: Corpus,
    reader: IndexReader,
    title: Field,
    tags: Field,
    body: Field,
}

/// 检索的一条结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hit {
    pub id: String,
    pub revision: i32,
    pub kind: Kind,
    pub scope: String,
    pub title: String,
    pub excerpt: String,
    /// BM25 分数乘 1000 取整。
    pub score: i64,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub kind: Option<Kind>,
    pub limit: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            kind: None,
            limit: 10,
        }
    }
}

impl SearchIndex {
    pub fn build(corpus: Corpus) -> Result<Self, Error> {
        Self::build_with(corpus, None)
    }

    /// `commit_every`：测试用，每写这么多条提交一次，造出多个段。
    fn build_with(corpus: Corpus, commit_every: Option<usize>) -> Result<Self, Error> {
        let mut sb = Schema::builder();
        let text = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqs),
        );
        let ord = sb.add_u64_field("ord", FAST);
        let title = sb.add_text_field("title", text.clone());
        let tags = sb.add_text_field("tags", text.clone());
        let body = sb.add_text_field("body", text);
        let tokenizers = TokenizerManager::default();
        tokenizers.register(TOKENIZER, TextAnalyzer::from(TcmsTokenizer::default()));
        let builder = Index::builder().schema(sb.build()).tokenizers(tokenizers);
        let doc = |i: usize, item: &CorpusItem| {
            let mut doc = TantivyDocument::default();
            doc.add_u64(ord, i as u64);
            doc.add_text(title, &item.title);
            for t in &item.tags {
                doc.add_text(tags, t);
            }
            doc.add_text(body, &item.body);
            doc
        };
        let index = match commit_every {
            // 平常：在当前线程里直接写成一个段，不起 tantivy 的后台线程。
            None => {
                let mut writer =
                    builder.single_segment_index_writer(RamDirectory::create(), WRITER_MEMORY)?;
                for (i, item) in corpus.items().iter().enumerate() {
                    writer.add_document(doc(i, item))?;
                }
                writer.finalize()?
            }
            // 测试：用普通的写入器分几次提交，造出多个段，再合成一个。
            Some(n) => {
                let index = builder.create_in_ram()?;
                let mut writer: IndexWriter = index.writer_with_num_threads(1, WRITER_MEMORY)?;
                writer.set_merge_policy(Box::new(NoMergePolicy));
                for (i, item) in corpus.items().iter().enumerate() {
                    writer.add_document(doc(i, item))?;
                    if (i + 1) % n == 0 {
                        writer.commit()?;
                    }
                }
                writer.commit()?;
                let segments = index.searchable_segment_ids()?;
                if segments.len() > 1 {
                    writer.merge(&segments).wait()?;
                }
                writer.wait_merging_threads()?;
                index
            }
        };

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self {
            corpus,
            reader,
            title,
            tags,
            body,
        })
    }

    pub fn corpus(&self) -> &Corpus {
        &self.corpus
    }

    /// 按 `query` 给条目打分，只返回命中的：`(条目在 corpus 里的位置, 量化后的分数)`，按位置排。
    pub fn score(&self, query: &str) -> Result<Vec<(usize, i64)>, Error> {
        let terms = distinct_terms(query, MAX_QUERY_TERMS);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(terms.len() * 3);
        for t in &terms {
            for (field, boost) in [
                (self.title, TITLE_BOOST),
                (self.tags, TAGS_BOOST),
                (self.body, 1.0),
            ] {
                let q = TermQuery::new(
                    Term::from_field_text(field, t),
                    IndexRecordOption::WithFreqs,
                );
                clauses.push((Occur::Should, Box::new(BoostQuery::new(Box::new(q), boost))));
            }
        }
        let searcher = self.reader.searcher();
        let mut hits = searcher.search(&BooleanQuery::new(clauses), &AllScores)?;
        hits.sort_unstable_by_key(|&(ord, _)| ord);
        Ok(hits
            .into_iter()
            .map(|(ord, s)| (ord as usize, quantize(s)))
            .collect())
    }

    /// `context_search`：可见范围里命中的条目，按分数、地位、更新时间、id 排。
    pub fn search(
        &self,
        query: &str,
        visibility: &Visibility,
        options: &SearchOptions,
    ) -> Result<Vec<Hit>, Error> {
        let items = self.corpus.items();
        let mut hits: Vec<(&CorpusItem, i64)> = self
            .score(query)?
            .into_iter()
            .map(|(i, s)| (&items[i], s))
            .filter(|(it, _)| {
                visibility.allows(&it.scope) && options.kind.is_none_or(|k| it.kind == k)
            })
            .collect();
        hits.sort_by(|a, b| (Reverse(a.1), a.0.tie_key()).cmp(&(Reverse(b.1), b.0.tie_key())));
        hits.truncate(options.limit.clamp(1, MAX_LIMIT));
        let terms: HashSet<String> = distinct_terms(query, MAX_QUERY_TERMS).into_iter().collect();
        Ok(hits
            .into_iter()
            .map(|(it, score)| Hit {
                id: it.id.clone(),
                revision: it.revision,
                kind: it.kind,
                scope: it.scope.to_string(),
                title: it.title.clone(),
                excerpt: excerpt(&it.body, &terms),
                score,
            })
            .collect())
    }
}

fn quantize(score: Score) -> i64 {
    (f64::from(score) * 1000.0).round() as i64
}

/// 正文里第一个命中词附近的一段，换行换成空格。正文没命中（只命中标题、标签）时取开头。
pub fn excerpt(body: &str, terms: &HashSet<String>) -> String {
    let hit_at = tokenize(body)
        .into_iter()
        .find(|p| terms.contains(&p.text))
        .map_or(0, |p| p.from);
    // 命中位置前面留几个字。
    let lead_start = body[..hit_at]
        .char_indices()
        .rev()
        .nth(EXCERPT_LEAD - 1)
        .map_or(0, |(i, _)| i);
    let start = if hit_at == 0 { 0 } else { lead_start };
    let rest = &body[start..];
    let end = rest
        .char_indices()
        .nth(EXCERPT_CHARS)
        .map_or(body.len(), |(i, _)| start + i);
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    let mut last_space = false;
    for c in body[start..end].chars() {
        let c = if c.is_whitespace() { ' ' } else { c };
        if c == ' ' && last_space {
            continue;
        }
        last_space = c == ' ';
        out.push(c);
    }
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    if end < body.len() {
        out.push('…');
    }
    out
}

/// 收集所有命中的文档和分数，不剪枝，所以结果和段的布局无关。
struct AllScores;

struct AllScoresSegment {
    ord: Column<u64>,
    out: Vec<(u64, Score)>,
}

impl Collector for AllScores {
    type Fruit = Vec<(u64, Score)>;
    type Child = AllScoresSegment;

    fn for_segment(
        &self,
        _segment: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<AllScoresSegment> {
        Ok(AllScoresSegment {
            ord: reader.fast_fields().u64("ord")?,
            out: Vec::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        true
    }

    fn merge_fruits(&self, fruits: Vec<Vec<(u64, Score)>>) -> tantivy::Result<Self::Fruit> {
        Ok(fruits.into_iter().flatten().collect())
    }
}

impl SegmentCollector for AllScoresSegment {
    type Fruit = Vec<(u64, Score)>;

    fn collect(&mut self, doc: DocId, score: Score) {
        if let Some(ord) = self.ord.first(doc) {
            self.out.push((ord, score));
        }
    }

    fn harvest(self) -> Self::Fruit {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, scope: &str, kind: Kind, title: &str, body: &str) -> CorpusItem {
        CorpusItem {
            id: id.to_owned(),
            scope: scope.parse().unwrap(),
            kind,
            authority: kind.authority(),
            title: title.to_owned(),
            body: body.to_owned(),
            tags: Vec::new(),
            owner: None,
            review_by: None,
            supersedes: None,
            revision: 1,
            path: "x.md".to_owned(),
            updated_ms: 0,
        }
    }

    fn sample() -> Corpus {
        let mut tagged = item(
            "ctx_03",
            "team:research",
            Kind::Fact,
            "数据来源",
            "CPI 和 PPI 用国家统计局的数，美国的数用 FRED。",
        );
        tagged.tags = vec!["数据".into(), "来源".into()];
        Corpus::new(
            vec![
                item(
                    "ctx_01",
                    "team:research",
                    Kind::Procedure,
                    "周报格式",
                    "周报用表格，日期写全，数字注明来源和口径。",
                ),
                item(
                    "ctx_02",
                    "role:macro",
                    Kind::Decision,
                    "宏观周报只写国内",
                    "2026 年起宏观周报只覆盖国内数据，海外放月报。",
                ),
                tagged,
                item(
                    "ctx_04",
                    "role:data",
                    Kind::Fact,
                    "周报的数据口径",
                    "数据岗位自己的周报口径。",
                ),
                item(
                    "ctx_05",
                    "org",
                    Kind::Reference,
                    "公司简介",
                    "我们做宏观研究。",
                ),
            ],
            7,
            Some("abc".into()),
        )
    }

    fn vis() -> Visibility {
        Visibility::agent("macro", Some("research"), None).unwrap()
    }

    #[test]
    fn finds_and_filters() {
        let idx = SearchIndex::build(sample()).unwrap();
        let hits = idx
            .search("周报格式", &vis(), &SearchOptions::default())
            .unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids[0], "ctx_01");
        assert!(ids.contains(&"ctx_02"));
        // 别的岗位的条目看不到。
        assert!(!ids.contains(&"ctx_04"));
        assert!(hits.iter().all(|h| h.score > 0));

        let all = idx
            .search("周报格式", &Visibility::All, &SearchOptions::default())
            .unwrap();
        assert!(all.iter().any(|h| h.id == "ctx_04"));

        let only = SearchOptions {
            kind: Some(Kind::Decision),
            limit: 10,
        };
        let hits = idx.search("周报", &vis(), &only).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "ctx_02");

        assert!(
            idx.search("，。", &vis(), &SearchOptions::default())
                .unwrap()
                .is_empty()
        );
        assert!(
            idx.search("没有的词", &vis(), &SearchOptions::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn tags_and_english_words() {
        let idx = SearchIndex::build(sample()).unwrap();
        let hits = idx
            .search("fred", &vis(), &SearchOptions::default())
            .unwrap();
        assert_eq!(hits[0].id, "ctx_03");
        assert!(hits[0].excerpt.contains("FRED"), "{}", hits[0].excerpt);
        let hits = idx
            .search("来源", &vis(), &SearchOptions::default())
            .unwrap();
        // 标签和标题都命中的排在只有正文命中的前面。
        assert_eq!(hits[0].id, "ctx_03");
    }

    #[test]
    fn reproducible_scores() {
        let a = SearchIndex::build(sample()).unwrap();
        let mut items = sample().items().to_vec();
        items.reverse();
        let b = SearchIndex::build(Corpus::new(items, 7, None)).unwrap();
        for q in ["周报格式", "数据 来源 口径", "宏观"] {
            assert_eq!(a.score(q).unwrap(), b.score(q).unwrap());
            assert_eq!(
                a.search(q, &Visibility::All, &SearchOptions::default())
                    .unwrap(),
                b.search(q, &Visibility::All, &SearchOptions::default())
                    .unwrap()
            );
        }
    }

    #[test]
    fn many_segments_merge_into_one() {
        // 条目多、内存不够一次写完时 tantivy 会写出好几个段；建完要合成一个，分数和一次写完的一样。
        let items: Vec<CorpusItem> = (0..1000)
            .map(|i| {
                item(
                    &format!("ctx_{i:05}"),
                    "org",
                    Kind::Fact,
                    &format!("条目 {i}"),
                    &format!(
                        "{i} 号说明：周报口径。{}",
                        "宏观研究月报季报".repeat(i % 7 + 1)
                    ),
                )
            })
            .collect();
        let split =
            SearchIndex::build_with(Corpus::new(items.clone(), 0, None), Some(100)).unwrap();
        assert_eq!(split.reader.searcher().segment_readers().len(), 1);
        let whole = SearchIndex::build(Corpus::new(items, 0, None)).unwrap();
        for q in ["口径", "宏观 季报", "42 号"] {
            assert_eq!(split.score(q).unwrap(), whole.score(q).unwrap());
        }
        assert_eq!(split.score("口径").unwrap().len(), 1000);
        assert_eq!(split.corpus().get("ctx_00042").unwrap().title, "条目 42");
        assert!(split.corpus().get("ctx_x").is_none());
    }

    #[test]
    fn excerpts() {
        let terms: HashSet<String> = ["口径".to_owned()].into();
        let body = format!("{}口径{}", "前".repeat(50), "后".repeat(200));
        let e = excerpt(&body, &terms);
        assert!(e.starts_with('…') && e.ends_with('…'));
        assert!(e.contains("口径"));
        assert_eq!(e.chars().filter(|&c| c == '前').count(), EXCERPT_LEAD);
        assert_eq!(
            excerpt("第一行\n\n第二行", &HashSet::new()),
            "第一行 第二行"
        );
        assert_eq!(excerpt("", &terms), "");
    }
}
