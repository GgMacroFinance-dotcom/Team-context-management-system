//! 基准用的数据：内容有变化的上下文仓库（中文词、英文词、数字，常见词出现得多）。
//! 随机数种子固定，每次造的内容一样（id 除外）。

use std::fmt::Write as _;

use anyhow::Result;
use tcms_core::{ItemId, Kind, TEAMS_FILE};
use tcms_git::{Author, ContextRepo, FileEdit};

use super::{ITEMS_PER_FILE, scope_dir};

/// 固定种子的 xorshift64*，不引入随机数库。
pub struct Rng(u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// [0, 1) 的小数。
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    /// 偏向前面的下标：常见词出现得多，像真的文字。
    pub fn skewed(&mut self, n: usize) -> usize {
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

pub struct Gen {
    pub rng: Rng,
    vocab: Vec<String>,
}

impl Gen {
    pub fn new() -> Self {
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

    pub fn word(&mut self) -> String {
        let i = self.rng.skewed(self.vocab.len());
        self.vocab[i].clone()
    }

    /// `n` 个词拼成一段：中文词直接相连，英文词前后加空格，偶尔加标点。
    pub fn text(&mut self, n: usize) -> String {
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

    pub fn item(&mut self, id: &ItemId) -> String {
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
/// 造一个 `items` 条的上下文仓库，做成一个提交。返回临时目录（丢了就删）、仓库、接着用的生成器和文件数。
pub fn make_repo(items: usize) -> Result<(tempfile::TempDir, ContextRepo, Gen, usize)> {
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
            path: file_path(f),
            content: Some(text.into_bytes()),
        });
    }
    repo.commit(&edits, "基准数据", &Author::default())?;
    Ok((dir, repo, gen_, files))
}

/// 第 `f` 个文件在仓库里的路径。
pub fn file_path(f: usize) -> String {
    format!("{}/f{f:05}.md", scope_dir(f))
}
