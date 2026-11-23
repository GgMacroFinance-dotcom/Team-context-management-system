//! 写入前的密钥脱敏（`docs/spec.md` 3.4）：agent 的提议进库之前过一遍。
//!
//! 只认形状明显的密钥，认不出来的靠人审。规则和 I.P. 的 `redact` 对齐（`sk-`、AWS、Slack、GitHub、JWT、
//! PEM 私钥、Bearer、`名字=值`、URL 里的密码），多认本系统自己的令牌（`tcms_` 加 64 位十六进制）。

use std::sync::LazyLock;

use regex::{Captures, Regex};

/// 换上去的记号。
pub const MARK: &str = "[已脱敏]";

fn re(p: &str) -> Regex {
    Regex::new(p).expect("内置的脱敏规则是合法的正则")
}

/// 整段换掉的。
static WHOLE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // 本系统的令牌。
        r"\btcms_[0-9a-f]{64}\b",
        // OpenAI、Anthropic 等 `sk-` 开头的。
        r"\bsk-[A-Za-z0-9_\-]{16,}",
        // AWS access key id。
        r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
        // Slack。
        r"\bxox[abeprs]-[A-Za-z0-9\-]{10,}",
        r"\bxapp-[A-Za-z0-9\-]{10,}",
        // GitHub。
        r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})",
        // Google API key。
        r"\bAIza[0-9A-Za-z_\-]{35}",
        // JWT。
        r"\beyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
        // PEM 私钥。
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
    ]
    .iter()
    .map(|p| re(p))
    .collect()
});

/// `Bearer xxx`：留下 Bearer。
static BEARER: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\b(bearer\s+)[A-Za-z0-9._~+/=\-]{8,}"));

/// `名字 = 值`、`名字: 值`，名字里带 key、token、secret、password：留下名字。
static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"(?i)\b([A-Za-z0-9_\-]*(?:api[_\-]?key|apikey|access[_\-]?key|token|secret|password|passwd|pwd)[A-Za-z0-9_\-]*)(["']?\s*[:=]\s*["']?)([^\s"',;]{6,})"#,
    )
});

/// URL 里的用户名和密码：`postgres://user:pass@host`。
static USERINFO: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b([a-z][a-z0-9+.\-]*://)[^\s/@:]+:[^\s/@]+@"));

/// 抹掉形状明显的密钥。返回抹过的文本和抹了几处。
pub fn redact(text: &str) -> (String, usize) {
    let mut out = text.to_owned();
    let mut n = 0;
    for r in WHOLE.iter() {
        let found = r.find_iter(&out).count();
        if found > 0 {
            n += found;
            out = r.replace_all(&out, MARK).into_owned();
        }
    }
    let found = BEARER.find_iter(&out).count();
    if found > 0 {
        n += found;
        out = BEARER
            .replace_all(&out, format!("${{1}}{MARK}"))
            .into_owned();
    }
    let mut assigned = 0;
    out = ASSIGNMENT
        .replace_all(&out, |c: &Captures<'_>| {
            // 值已经被上面换掉的不再算一次。
            if c[3].starts_with(MARK) {
                c[0].to_owned()
            } else {
                assigned += 1;
                format!("{}{}{MARK}", &c[1], &c[2])
            }
        })
        .into_owned();
    n += assigned;
    let found = USERINFO.find_iter(&out).count();
    if found > 0 {
        n += found;
        out = USERINFO
            .replace_all(&out, format!("${{1}}{MARK}@"))
            .into_owned();
    }
    (out, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_go_and_ordinary_text_stays() {
        let token = format!("tcms_{}", "ab".repeat(32));
        for (input, gone) in [
            (format!("令牌是 {token}"), token.as_str()),
            (
                "key 是 sk-proj-abcdefghijklmnop1234".to_owned(),
                "abcdefghijklmnop",
            ),
            (
                "Authorization: Bearer abc.DEF-123_456xyz".to_owned(),
                "abc.DEF-123_456xyz",
            ),
            (
                "export OPENAI_API_KEY=live_9f8e7d6c5b4a".to_owned(),
                "live_9f8e7d6c5b4a",
            ),
            ("password: hunter2hunter2".to_owned(), "hunter2hunter2"),
            ("AKIAIOSFODNN7EXAMPLE".to_owned(), "IOSFODNN7"),
            (
                "ghp_abcdefghijklmnopqrstuvwxyz0123".to_owned(),
                "abcdefghijklmnop",
            ),
            (
                "postgres://admin:s3cretpw@db.internal/app".to_owned(),
                "s3cretpw",
            ),
            (
                "-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----".to_owned(),
                "MIIEow",
            ),
        ] {
            let (out, n) = redact(&input);
            assert!(
                !out.contains(gone) && out.contains(MARK) && n >= 1,
                "{input} → {out}"
            );
        }
        assert_eq!(
            redact("MACRO_API_KEY=abcdef123456"),
            ("MACRO_API_KEY=[已脱敏]".to_owned(), 1)
        );
        assert_eq!(redact("Bearer abcdefgh12345678").0, "Bearer [已脱敏]");
        for plain in [
            "美国 10 年期国债收益率 4.88%，数据截至 2026-11-09。",
            "用 get_fred_series 取 DGS10，token 用量 1200。",
            "密钥放在环境变量 MACRO_API_KEY 里。",
            "https://fred.stlouisfed.org/series/DGS10",
            "password 这个词本身不是密钥",
        ] {
            assert_eq!(redact(plain), (plain.to_owned(), 0));
        }
    }
}
