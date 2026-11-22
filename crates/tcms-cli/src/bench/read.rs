//! `tcms bench read`：造一个内容有变化的上下文仓库（中文词、英文词、数字，常见词出现得多），
//! 索引进临时 schema，然后量：从 PostgreSQL 取出全部生效条目、建检索索引、编上下文包、检索。
//! 编包和检索各跑若干次，报中位数和 p95。随机数种子固定，每次造的内容一样（id 除外）。

use std::fmt::Write as _;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use tcms_core::{ItemId, Kind, TEAMS_FILE};
use tcms_git::{Author, ContextRepo, FileEdit};
use tcms_pack::{Request, compile};
use tcms_search::{Corpus, SearchIndex, SearchOptions, Visibility};
use tcms_store::Store;

use super::{ITEMS_PER_FILE, ms, scope_dir, temp_schema};

/// 编一个上下文包的 p95 门槛（1 万条、8 千 token）。
const PACK_P95_LIMIT: Duration = Duration::from_millis(20);
/// 检索的 p95 门槛（1 万条）。
const SEARCH_P95_LIMIT: Duration = Duration::from_millis(10);
/// 判断复核日期用的“今天”，固定下来，每次结果一样。
const TODAY: &str = "2026-11-23";

/// 固定种子的 xorshift64*，不引入随机数库。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// [0, 1) 的小数。
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    /// 偏向前面的下标：常见词出现得多，像真的文字。
    fn skewed(&mut self, n: usize) -> usize {
        ((self.unit().powi(3)) * n as f64) as usize
    }
}

const CHARS: &str = "的一是在不了有和人这中大为上个国我以要他时来用们生到作地于出就分对成会可主发年动同工也能下过子说产种面而方后多定行学法所民得经十三之进着等部度家电力里如水化高自二理起小物现实加量都两体制机当使点从业本去把性好应开它合还因由其些然前外天政四日那社义事平形相全表间样与关各重新线内数正心反你明看原又么利比或但质气第向道命此变条只没结解问意建月公无系军很情者最立代想已通并提直题党程展五果料象员革位入常文总次品式活设及管特件长求老头基资边流路级少图山统接知较将组见计别她手角期根论运农指几九区强放决西被干做必战先回则任取据处府";

const WORDS: [&str; 40] = [
    "cpi",
    "ppi",
    "pmi",
    "gdp",
    "m2",
    "fred",
    "api",
    "report",
    "weekly",
    "monthly",
    "rate",
    "policy",
    "index",
    "yield",
    "bond",
    "credit",
    "export",
    "import",
    "china",
    "us",
    "data",
    "source",
    "review",
    "model",
    "agent",
    "team",
    "role",
    "project",
    "table",
    "chart",
    "q1",
    "q2",
    "q3",
    "q4",
    "2025",
    "2026",
    "inflation",
    "growth",
    "risk",
    "forecast",
];

struct Gen {
    rng: Rng,
    vocab: Vec<String>,
}

impl Gen {
    fn new() -> Self {
        let mut rng = Rng(0x7C3E_9D1A_5B2F_4E61);
        let chars: Vec<char> = CHARS.chars().collect();
        let mut vocab: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        while vocab.len() < 1500 {
            let w: String = [chars[rng.below(chars.len())], chars[rng.below(chars.len())]]
                .into_iter()
                .collect();
            if seen.insert(w.clone()) {
                vocab.push(w);
            }
        }
        // 英文词插在中间，常见程度和中文词混在一起。
        for (i, w) in WORDS.iter().enumerate() {
            vocab.insert(i * 30 % vocab.len(), (*w).to_owned());
        }
        Self { rng, vocab }
    }

    fn word(&mut self) -> String {
        let i = self.rng.skewed(self.vocab.len());
        self.vocab[i].clone()
    }

    /// `n` 个词拼成一段：中文词直接相连，英文词前后加空格，偶尔加标点。
    fn text(&mut self, n: usize) -> String {
        let mut s = String::new();
        for k in 0..n {
            let w = self.word();
            if w.is_ascii() {
                s.push(' ');
                s.push_str(&w);
                s.push(' ');
            } else {
                s.push_str(&w);
            }
            if k + 1 < n {
                match self.rng.below(12) {
                    0 => s.push('，'),
                    1 => s.push('。'),
                    _ => {}
                }
            }
        }
        s.split_whitespace().collect::<Vec<_>>().join(" ") + "。"
    }

    fn item(&mut self, id: &ItemId) -> String {
        let kind = if self.rng.unit() < 0.003 {
            Kind::Rule
        } else {
            [
                Kind::Decision,
                Kind::Fact,
                Kind::Term,
                Kind::Procedure,
                Kind::Reference,
                Kind::Memory,
            ][self.rng.below(6)]
        };
        let mut tags: Vec<String> = (0..self.rng.below(3)).map(|_| self.word()).collect();
        if self.rng.unit() < 0.002 {
            tags.push("pinned".to_owned());
        }
        let mut meta = format!("id={id} kind={kind}");
        if !tags.is_empty() {
            let _ = write!(meta, " tags=\"{}\"", tags.join(", "));
        }
        if self.rng.unit() < 0.1 {
            let _ = write!(
                meta,
                " review_by=2026-{:02}-{:02}",
                self.rng.range(1, 12),
                self.rng.range(1, 28)
            );
        }
        let title_len = self.rng.range(2, 5);
        let title = self.text(title_len);
        let body_len = self.rng.range(30, 160);
        let body = self.text(body_len);
        format!(
            "\n## {}\n<!-- ctx {meta} -->\n\n{body}\n",
            title.trim_end_matches('。')
        )
    }
}

fn teams_yaml() -> String {
    let mut s = String::from("teams:\n");
    for t in 0..5 {
        let roles: Vec<String> = (0..10)
            .filter(|r| r % 5 == t)
            .map(|r| format!("r{r}"))
            .collect();
        let _ = writeln!(s, "  t{t}:\n    roles: [{}]", roles.join(", "));
    }
    s.push_str("projects: [p0, p1, p2]\n");
    s
}

pub async fn run(items: usize, requests: usize, budget: usize, check: bool) -> Result<ExitCode> {
    ensure!(items > 0 && requests > 0, "条数和次数都要大于 0");
    let url = crate::database_url()?;
    let files = items.div_ceil(ITEMS_PER_FILE);
    let dir = tempfile::tempdir()?;
    let repo = ContextRepo::init(dir.path())?;
    let mut gen_ = Gen::new();
    let mut edits = vec![FileEdit {
        path: TEAMS_FILE.to_owned(),
        content: Some(teams_yaml().into_bytes()),
    }];
    for f in 0..files {
        let n = ITEMS_PER_FILE.min(items - f * ITEMS_PER_FILE);
        let mut text = format!("# 文件 {f}\n");
        for _ in 0..n {
            text.push_str(&gen_.item(&ItemId::generate()));
        }
        edits.push(FileEdit {
            path: format!("{}/f{f:05}.md", scope_dir(f)),
            content: Some(text.into_bytes()),
        });
    }
    repo.commit(&edits, "基准数据", &Author::default())?;

    let store = Store::connect(&url, &temp_schema())
        .await
        .context("连接 PostgreSQL")?;
    let result = measure(&repo, &store, &mut gen_, requests, budget).await;
    store.drop_schema().await?;
    let m = result?;

    println!(
        "读的基准：{items} 条、{files} 个文件，编包 {requests} 次（预算 {budget} token），检索 {requests} 次"
    );
    println!("  从 PostgreSQL 取出生效条目 {:>8.1} 毫秒", ms(m.load));
    println!("  建检索索引               {:>8.1} 毫秒", ms(m.build));
    println!(
        "  编上下文包     中位数 {:>6.2} 毫秒   p95 {:>6.2} 毫秒   门槛 p95 {:.0} 毫秒",
        ms(m.pack.p50),
        ms(m.pack.p95),
        ms(PACK_P95_LIMIT)
    );
    println!(
        "  检索           中位数 {:>6.2} 毫秒   p95 {:>6.2} 毫秒   门槛 p95 {:.0} 毫秒",
        ms(m.search.p50),
        ms(m.search.p95),
        ms(SEARCH_P95_LIMIT)
    );
    println!(
        "  取条目和建索引多用的内存 {}（进程物理内存前后差）",
        m.memory
    );
    println!(
        "  平均每个包：可见 {:.0} 条，全文 {:.1} 条，目录 {:.1} 条，约 {:.0} token",
        m.visible, m.included, m.listed, m.tokens
    );

    let mut failed = false;
    if m.pack.p95 > PACK_P95_LIMIT {
        eprintln!("编上下文包超过门槛");
        failed = true;
    }
    if m.search.p95 > SEARCH_P95_LIMIT {
        eprintln!("检索超过门槛");
        failed = true;
    }
    Ok(if check && failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

struct Percentiles {
    p50: Duration,
    p95: Duration,
}

fn percentiles(mut d: Vec<Duration>) -> Percentiles {
    d.sort();
    let at = |q: f64| d[((d.len() as f64 * q).ceil() as usize).clamp(1, d.len()) - 1];
    Percentiles {
        p50: at(0.5),
        p95: at(0.95),
    }
}

struct Measured {
    load: Duration,
    build: Duration,
    pack: Percentiles,
    search: Percentiles,
    visible: f64,
    included: f64,
    listed: f64,
    tokens: f64,
    /// 取条目、建索引前后进程物理内存的差。
    memory: String,
}

fn identity(i: usize) -> Result<Visibility> {
    let role = format!("r{}", i % 10);
    let team = format!("t{}", i % 5);
    let project = i.is_multiple_of(2).then(|| format!("p{}", i % 3));
    Ok(Visibility::agent(&role, Some(&team), project.as_deref())?)
}

async fn measure(
    repo: &ContextRepo,
    store: &Store,
    gen_: &mut Gen,
    requests: usize,
    budget: usize,
) -> Result<Measured> {
    let r = tcms_index::reindex(repo, store, false).await?;
    ensure!(
        r.warnings.is_empty(),
        "基准数据有提醒：{:?}",
        &r.warnings[..1]
    );

    let rss_before = rss_kb();
    let t = Instant::now();
    let corpus = Corpus::load(store).await?;
    let load = t.elapsed();
    let t = Instant::now();
    let index = SearchIndex::build(corpus)?;
    let build = t.elapsed();
    let memory = match (rss_before, rss_kb()) {
        (Some(a), Some(b)) => format!("约 {:.0} MB", b.saturating_sub(a) as f64 / 1024.0),
        _ => "未知".to_owned(),
    };

    let tasks: Vec<String> = (0..requests)
        .map(|_| {
            let n = gen_.rng.range(6, 30);
            gen_.text(n)
        })
        .collect();
    let queries: Vec<String> = (0..requests)
        .map(|_| {
            let n = gen_.rng.range(1, 3);
            (0..n).map(|_| gen_.word()).collect::<Vec<_>>().join(" ")
        })
        .collect();
    let ids: Vec<Visibility> = (0..requests).map(identity).collect::<Result<_>>()?;

    // 先热一下，再量。
    let req = |i: usize| Request {
        visibility: &ids[i],
        task: &tasks[i],
        budget_tokens: budget,
        today: TODAY,
    };
    let first = compile(&index, &req(0))?;
    ensure!(
        first == compile(&index, &req(0))?,
        "同一个请求两次编出来不一样"
    );

    let (mut pack_times, mut search_times) = (Vec::new(), Vec::new());
    let (mut included, mut listed, mut tokens, mut visible) = (0usize, 0usize, 0usize, 0usize);
    for (i, visibility) in ids.iter().enumerate() {
        let t = Instant::now();
        let p = compile(&index, &req(i))?;
        pack_times.push(t.elapsed());
        ensure!(
            p.prefix_tokens > budget || p.prefix_tokens + p.task_tokens <= budget,
            "包超出了预算"
        );
        included += p.included.len();
        listed += p.listed.len();
        tokens += p.prefix_tokens + p.task_tokens;
        visible += index
            .corpus()
            .items()
            .iter()
            .filter(|it| visibility.allows(&it.scope))
            .count();
    }
    for (query, visibility) in queries.iter().zip(&ids) {
        let t = Instant::now();
        let hits = index.search(query, visibility, &SearchOptions::default())?;
        search_times.push(t.elapsed());
        std::hint::black_box(hits);
    }
    let n = requests as f64;
    Ok(Measured {
        load,
        build,
        pack: percentiles(pack_times),
        search: percentiles(search_times),
        visible: visible as f64 / n,
        included: included as f64 / n,
        listed: listed as f64 / n,
        tokens: tokens as f64 / n,
        memory,
    })
}

/// 进程当前占用的物理内存（KB）。只在 Linux 上有（读 /proc/self/status）。
fn rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}
