//! 分词（`docs/architecture.md` 第 6 节）：英文和数字按词、转小写；中日韩文字按相邻两字（bigram），
//! 只有一个字时就是这一个字。标点和空白只当分隔。不用词典，所以不需要加载时间和额外内存。
//!
//! 建索引和查询用同一套规则，摘录也靠它找命中的位置。

use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

/// 注册到 tantivy 时用的名字。
pub const TOKENIZER: &str = "tcms";

/// 超过这个字节数的英文词不进索引（多半是哈希、base64 之类）。
const MAX_WORD_BYTES: usize = 64;

/// 中日韩文字：汉字（含扩展区和兼容区）、平假名、片假名、韩文音节，以及“々〆〇”。
/// 全角标点不算，它们只当分隔。
pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3005..=0x3007
        | 0x3040..=0x30FF
        | 0x31F0..=0x31FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xAC00..=0xD7AF
        | 0xF900..=0xFAFF
        | 0x20000..=0x2FA1F
        | 0x30000..=0x323AF)
}

/// tantivy 用的分词器。
#[derive(Clone, Default)]
pub struct TcmsTokenizer {
    token: Token,
}

impl Tokenizer for TcmsTokenizer {
    type TokenStream<'a> = TcmsTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> TcmsTokenStream<'a> {
        self.token.reset();
        TcmsTokenStream {
            text,
            pos: 0,
            prev_cjk: None,
            run_len: 0,
            token: &mut self.token,
        }
    }
}

pub struct TcmsTokenStream<'a> {
    text: &'a str,
    /// 下一个要看的字节位置。
    pos: usize,
    /// 当前这段中日韩文字里上一个字的字节位置。
    prev_cjk: Option<usize>,
    /// 当前这段中日韩文字已经看了几个字。
    run_len: usize,
    token: &'a mut Token,
}

impl TcmsTokenStream<'_> {
    fn emit(&mut self, from: usize, to: usize, lowercase: bool) {
        self.token.text.clear();
        let s = &self.text[from..to];
        if lowercase {
            self.token
                .text
                .extend(s.chars().flat_map(char::to_lowercase));
        } else {
            self.token.text.push_str(s);
        }
        self.token.offset_from = from;
        self.token.offset_to = to;
        self.token.position = self.token.position.wrapping_add(1);
        self.token.position_length = 1;
    }

    /// 一段中日韩文字只有一个字时，在这段结束时把这个字单独输出。
    fn end_cjk_run(&mut self) -> bool {
        let single = (self.run_len == 1).then_some(self.prev_cjk).flatten();
        self.prev_cjk = None;
        self.run_len = 0;
        if let Some(from) = single {
            let to = from + self.text[from..].chars().next().map_or(0, char::len_utf8);
            self.emit(from, to, false);
            return true;
        }
        false
    }
}

impl TokenStream for TcmsTokenStream<'_> {
    fn advance(&mut self) -> bool {
        loop {
            let Some(c) = self.text[self.pos..].chars().next() else {
                return self.end_cjk_run();
            };
            let at = self.pos;
            if is_cjk(c) {
                self.pos += c.len_utf8();
                self.run_len += 1;
                if let Some(prev) = self.prev_cjk.replace(at) {
                    self.emit(prev, self.pos, false);
                    return true;
                }
                continue;
            }
            // 不是中日韩文字：先结束前面那段（只有一个字时输出它，这个字符留到下一轮再看）。
            if self.end_cjk_run() {
                return true;
            }
            if c.is_alphanumeric() {
                let end = self.text[at..]
                    .char_indices()
                    .find(|&(_, ch)| !ch.is_alphanumeric() || is_cjk(ch))
                    .map_or(self.text.len(), |(i, _)| at + i);
                self.pos = end;
                if end - at <= MAX_WORD_BYTES {
                    self.emit(at, end, true);
                    return true;
                }
                continue;
            }
            self.pos += c.len_utf8();
        }
    }

    fn token(&self) -> &Token {
        self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        self.token
    }
}

/// 一个词：文本和在原文里的字节范围。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    pub text: String,
    pub from: usize,
    pub to: usize,
}

/// 把一段文字切成词（查询和摘录用）。
pub fn tokenize(text: &str) -> Vec<Piece> {
    let mut tokenizer = TcmsTokenizer::default();
    let mut stream = tokenizer.token_stream(text);
    let mut out = Vec::new();
    while stream.advance() {
        let t = stream.token();
        out.push(Piece {
            text: t.text.clone(),
            from: t.offset_from,
            to: t.offset_to,
        });
    }
    out
}

/// 不重复的词，按第一次出现的顺序，最多 `limit` 个。
pub fn distinct_terms(text: &str, limit: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for p in tokenize(text) {
        if out.len() >= limit {
            break;
        }
        if seen.insert(p.text.clone()) {
            out.push(p.text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(s: &str) -> Vec<String> {
        tokenize(s).into_iter().map(|p| p.text).collect()
    }

    #[test]
    fn words_and_bigrams() {
        assert_eq!(
            texts("Q4报告，CPI走势 and PPI"),
            ["q4", "报告", "cpi", "走势", "and", "ppi"]
        );
        assert_eq!(texts("中国人民"), ["中国", "国人", "人民"]);
        assert_eq!(texts("税"), ["税"]);
        assert_eq!(texts("a税b"), ["a", "税", "b"]);
        assert_eq!(texts("税 率"), ["税", "率"]);
        assert_eq!(texts("「周报」用表格。"), ["周报", "用表", "表格"]);
        assert_eq!(texts("カタカナ"), ["カタ", "タカ", "カナ"]);
        assert_eq!(texts("Größe ÉTÉ"), ["größe", "été"]);
        assert!(texts("").is_empty());
        assert!(texts("，。！ ---").is_empty());
    }

    #[test]
    fn offsets_point_into_the_text() {
        let s = "看 CPI 走势";
        for p in tokenize(s) {
            assert_eq!(s[p.from..p.to].to_lowercase(), p.text);
        }
        let p = tokenize(s);
        assert_eq!(p[1].text, "cpi");
        assert_eq!(&s[p[1].from..p[1].to], "CPI");
    }

    #[test]
    fn long_words_are_skipped() {
        let long = "a".repeat(MAX_WORD_BYTES + 1);
        assert_eq!(texts(&format!("x {long} y")), ["x", "y"]);
    }

    #[test]
    fn distinct_keeps_first_order() {
        assert_eq!(distinct_terms("b a b c a", 10), ["b", "a", "c"]);
        assert_eq!(distinct_terms("b a b c a", 2), ["b", "a"]);
    }
}
