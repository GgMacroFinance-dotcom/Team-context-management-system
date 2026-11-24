//! 排序键：AFFiNE 给文件夹树、收藏这类列表排序用的字符串（按字节比大小）。
//!
//! 格式和 AFFiNE 0.27.5 一样（只读了 MIT 部分 `packages/common/infra/src/utils/fractional-indexing.ts`
//! 的设计，没复制代码）：先是 npm 包 `fractional-indexing` 那种“整数部分 + 小数部分”的键（62 进制，
//! 整数部分的第一个字母管长度），再接一个 `0` 和 32 个随机字符（不含 `0`），两边同时插到同一个位置
//! 也不会撞。这里按那个公开的算法自己写了一遍。
//!
//! 我们生成的键要能被 AFFiNE 再拿去当邻居生成新键，所以必须是合法的 `fractional-indexing` 键：
//! 小数部分不能以 `0` 结尾，整数部分的长度要和第一个字母对得上。

use std::cmp::Ordering;

/// 62 个数字，按字节从小到大。
const DIGITS: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
/// 随机后缀用的字符：没有 `0`，所以键不会以 `0` 结尾。
const POSTFIX_CHARS: &[u8; 61] = b"123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
/// 随机后缀的长度。
const POSTFIX_LEN: usize = 32;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OrderError {
    #[error("排序键不合法：{0:?}")]
    Invalid(String),
    #[error("前一个键 {0:?} 不小于后一个键 {1:?}")]
    NotIncreasing(String, String),
    #[error("排序键用完了")]
    Exhausted,
}

fn digit(c: u8) -> Option<usize> {
    DIGITS.iter().position(|&d| d == c)
}

/// 整数部分有几个字符（含第一个字母）。
fn integer_len(head: u8) -> Option<usize> {
    match head {
        b'a'..=b'z' => Some(usize::from(head - b'a') + 2),
        b'A'..=b'Z' => Some(usize::from(b'Z' - head) + 2),
        _ => None,
    }
}

/// 最小的整数部分：`A` 加 26 个 `0`。比它还小的键生成不出来。
fn smallest_integer() -> String {
    let mut s = String::from("A");
    s.push_str(&"0".repeat(26));
    s
}

fn integer_part(key: &str) -> Result<&str, OrderError> {
    let bytes = key.as_bytes();
    let len = bytes
        .first()
        .and_then(|&h| integer_len(h))
        .ok_or_else(|| OrderError::Invalid(key.to_owned()))?;
    if len > bytes.len() || !bytes[1..len].iter().all(|&c| digit(c).is_some()) {
        return Err(OrderError::Invalid(key.to_owned()));
    }
    Ok(&key[..len])
}

/// 是不是一个合法的 `fractional-indexing` 键。
fn validate(key: &str) -> Result<(), OrderError> {
    if key == smallest_integer() {
        return Err(OrderError::Invalid(key.to_owned()));
    }
    let int = integer_part(key)?;
    let frac = &key.as_bytes()[int.len()..];
    if frac.last() == Some(&b'0') || !frac.iter().all(|&c| digit(c).is_some()) {
        return Err(OrderError::Invalid(key.to_owned()));
    }
    Ok(())
}

/// 两个小数部分中间的一个（`b` 为空表示没有上界）。两个都不能以 `0` 结尾。
fn midpoint(a: &[u8], b: Option<&[u8]>) -> Vec<u8> {
    if let Some(b) = b {
        // 共同的前缀原样留着（`a` 比 `b` 短时当成后面补 `0`）。
        let mut n = 0;
        while n < b.len() && a.get(n).copied().unwrap_or(b'0') == b[n] {
            n += 1;
        }
        if n > 0 {
            let mut out = b[..n].to_vec();
            out.extend(midpoint(a.get(n..).unwrap_or(&[]), Some(&b[n..])));
            return out;
        }
    }
    let da = a.first().and_then(|&c| digit(c)).unwrap_or(0);
    let db = b
        .and_then(|b| b.first())
        .and_then(|&c| digit(c))
        .unwrap_or(DIGITS.len());
    if db - da > 1 {
        // 和 JS 的 Math.round(0.5 * (da + db)) 一样：.5 往上取。
        vec![DIGITS[(da + db).div_ceil(2)]]
    } else if let Some(b) = b.filter(|b| b.len() > 1) {
        vec![b[0]]
    } else {
        let mut out = vec![DIGITS[da]];
        out.extend(midpoint(a.get(1..).unwrap_or(&[]), None));
        out
    }
}

fn increment_integer(x: &str) -> Option<String> {
    let bytes = x.as_bytes();
    let head = bytes[0];
    let mut digs = bytes[1..].to_vec();
    let mut carry = true;
    for d in digs.iter_mut().rev() {
        let next = digit(*d)? + 1;
        if next == DIGITS.len() {
            *d = b'0';
        } else {
            *d = DIGITS[next];
            carry = false;
            break;
        }
    }
    if carry {
        if head == b'Z' {
            return Some("a0".to_owned());
        }
        if head == b'z' {
            return None;
        }
        let h = head + 1;
        if h > b'a' {
            digs.push(b'0');
        } else {
            digs.pop();
        }
        let mut out = vec![h];
        out.extend(digs);
        return String::from_utf8(out).ok();
    }
    let mut out = vec![head];
    out.extend(digs);
    String::from_utf8(out).ok()
}

fn decrement_integer(x: &str) -> Option<String> {
    let bytes = x.as_bytes();
    let head = bytes[0];
    let last = DIGITS[DIGITS.len() - 1];
    let mut digs = bytes[1..].to_vec();
    let mut borrow = true;
    for d in digs.iter_mut().rev() {
        let i = digit(*d)?;
        if i == 0 {
            *d = last;
        } else {
            *d = DIGITS[i - 1];
            borrow = false;
            break;
        }
    }
    if borrow {
        if head == b'a' {
            return Some(format!("Z{}", last as char));
        }
        if head == b'A' {
            return None;
        }
        let h = head - 1;
        if h < b'Z' {
            digs.push(last);
        } else {
            digs.pop();
        }
        let mut out = vec![h];
        out.extend(digs);
        return String::from_utf8(out).ok();
    }
    let mut out = vec![head];
    out.extend(digs);
    String::from_utf8(out).ok()
}

fn concat(int: &str, frac: Vec<u8>) -> String {
    let mut s = int.to_owned();
    // `frac` 只会是 DIGITS 里的字符。
    s.push_str(std::str::from_utf8(&frac).unwrap_or_default());
    s
}

/// `fractional-indexing` 的 `generateKeyBetween`：`a < 结果 < b`，`None` 表示那一头没有邻居。
fn key_between(a: Option<&str>, b: Option<&str>) -> Result<String, OrderError> {
    if let Some(a) = a {
        validate(a)?;
    }
    if let Some(b) = b {
        validate(b)?;
    }
    match (a, b) {
        (None, None) => Ok("a0".to_owned()),
        (None, Some(b)) => {
            let ib = integer_part(b)?;
            let fb = &b.as_bytes()[ib.len()..];
            if ib == smallest_integer() {
                return Ok(concat(ib, midpoint(&[], Some(fb))));
            }
            if ib.len() < b.len() {
                return Ok(ib.to_owned());
            }
            decrement_integer(ib).ok_or(OrderError::Exhausted)
        }
        (Some(a), None) => {
            let ia = integer_part(a)?;
            let fa = &a.as_bytes()[ia.len()..];
            Ok(match increment_integer(ia) {
                Some(i) => i,
                None => concat(ia, midpoint(fa, None)),
            })
        }
        (Some(a), Some(b)) => {
            if a.cmp(b) != Ordering::Less {
                return Err(OrderError::NotIncreasing(a.to_owned(), b.to_owned()));
            }
            let ia = integer_part(a)?;
            let fa = &a.as_bytes()[ia.len()..];
            let ib = integer_part(b)?;
            let fb = &b.as_bytes()[ib.len()..];
            if ia == ib {
                return Ok(concat(ia, midpoint(fa, Some(fb))));
            }
            let i = increment_integer(ia).ok_or(OrderError::Exhausted)?;
            if i.as_str() < b {
                return Ok(i);
            }
            Ok(concat(ia, midpoint(fa, None)))
        }
    }
}

/// 随机后缀从哪里来（测试里换成固定的）。
pub trait Postfix {
    fn postfix(&mut self) -> String;
}

/// 系统随机数做的后缀。
#[derive(Debug, Default)]
pub struct RandomPostfix;

impl Postfix for RandomPostfix {
    fn postfix(&mut self) -> String {
        let mut buf = [0u8; POSTFIX_LEN];
        if getrandom::fill(&mut buf).is_err() {
            // 拿不到系统随机数时退回用时间拼：只是更容易和别人同时插的键撞上，顺序还是对的。
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            for (i, b) in buf.iter_mut().enumerate() {
                *b = (t >> ((i % 16) * 8)) as u8 ^ i as u8;
            }
        }
        buf.iter()
            .map(|b| POSTFIX_CHARS[usize::from(*b) % POSTFIX_CHARS.len()] as char)
            .collect()
    }
}

/// 去掉随机后缀剩下的部分。没有后缀（短于 33 个字符）的键原样返回，和 AFFiNE 一样。
fn subkey(key: &str) -> &str {
    if key.len() <= POSTFIX_LEN + 1 {
        key
    } else {
        &key[..key.len() - POSTFIX_LEN - 1]
    }
}

/// 在 `a`、`b` 两个排序键中间生成一个新键（`None` 表示放在最前或最后），带随机后缀。
pub fn key_between_with(
    a: Option<&str>,
    b: Option<&str>,
    postfix: &mut impl Postfix,
) -> Result<String, OrderError> {
    if let (Some(a), Some(b)) = (a, b)
        && a >= b
    {
        return Err(OrderError::NotIncreasing(a.to_owned(), b.to_owned()));
    }
    let sa = a.map(subkey);
    let sb = b.map(subkey);
    let base = match (sa, sb) {
        (Some(x), Some(y)) if x.starts_with(y) || y.starts_with(x) => {
            // 去掉后缀以后一样（两边同时插在同一处），只能在完整的键之间找。
            let (a, b) = (a.unwrap_or_default(), b.unwrap_or_default());
            let mut key = key_between(Some(a), Some(b))?;
            // 加上 `0` 和后缀以后不能跑到 `b` 后面去。
            while b.starts_with(&format!("{key}0")) {
                key = key_between(Some(&key), Some(b))?;
            }
            key
        }
        _ => key_between(sa, sb)?,
    };
    Ok(format!("{base}0{}", postfix.postfix()))
}

/// 用系统随机数做后缀的 [`key_between_with`]。
pub fn key_between_random(a: Option<&str>, b: Option<&str>) -> Result<String, OrderError> {
    key_between_with(a, b, &mut RandomPostfix)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按 `fractional-indexing` 公开的算法手算的几组结果。
    #[test]
    fn plain_keys_match_the_npm_package() {
        let cases: &[(Option<&str>, Option<&str>, &str)] = &[
            (None, None, "a0"),
            (None, Some("a0"), "Zz"),
            (None, Some("Zz"), "Zy"),
            (Some("a0"), None, "a1"),
            (Some("a1"), None, "a2"),
            (Some("a0"), Some("a1"), "a0V"),
            (Some("a1"), Some("a2"), "a1V"),
            (Some("a0V"), Some("a1"), "a0l"),
            (Some("Zz"), Some("a0"), "ZzV"),
            (Some("Zz"), Some("a1"), "a0"),
            (None, Some("Y00"), "Xzzz"),
            (Some("bzz"), None, "c000"),
            (Some("a0"), Some("a0V"), "a0G"),
            (Some("a0"), Some("a0G"), "a08"),
            (Some("b125"), Some("b129"), "b127"),
            (Some("a0"), Some("a1V"), "a1"),
            (Some("Zz"), Some("a01"), "a0"),
            (None, Some("a0V"), "a0"),
            (None, Some("b999"), "b99"),
            (Some("a0"), Some("a0001"), "a0000V"),
        ];
        for (a, b, want) in cases {
            assert_eq!(&key_between(*a, *b).unwrap(), want, "{a:?} {b:?}");
        }
    }

    #[test]
    fn invalid_keys_are_rejected() {
        assert!(key_between(Some("a00"), None).is_err());
        assert!(key_between(Some("a0"), Some("a0")).is_err());
        assert!(key_between(Some("a1"), Some("a0")).is_err());
        assert!(key_between(Some("b0"), None).is_err());
        assert!(key_between(None, Some(&smallest_integer())).is_err());
    }

    /// AFFiNE 自己的测试（`fractional-indexing.spec.ts` 的 “no postfix”）里写明的几条行为。
    #[test]
    fn behaves_like_affine_on_keys_without_postfix() {
        let mut p = RandomPostfix;
        let starts = |a: Option<&str>, b: Option<&str>, p: &mut RandomPostfix| {
            key_between_with(a, b, p).unwrap()
        };
        assert!(starts(Some("a0"), None, &mut p).starts_with("a1"));
        assert!(starts(Some("a01"), None, &mut p).starts_with("a1"));
        assert!(starts(Some("a0001"), None, &mut p).starts_with("a1"));
        assert!(starts(None, Some("a0"), &mut p).starts_with("Zz"));
        assert!(starts(Some("a0"), Some("a01"), &mut p).starts_with("a00V"));
    }

    struct Fixed(char);
    impl Postfix for Fixed {
        fn postfix(&mut self) -> String {
            self.0.to_string().repeat(POSTFIX_LEN)
        }
    }

    #[test]
    fn keys_with_postfix_sort_and_stay_valid() {
        let first = key_between_with(None, None, &mut Fixed('x')).unwrap();
        assert_eq!(first, format!("a00{}", "x".repeat(32)));
        validate(&first).unwrap();
        let after = key_between_with(Some(&first), None, &mut Fixed('x')).unwrap();
        assert!(after > first);
        let before = key_between_with(None, Some(&first), &mut Fixed('x')).unwrap();
        assert!(before < first);
        let mid = key_between_with(Some(&first), Some(&after), &mut Fixed('x')).unwrap();
        assert!(first < mid && mid < after, "{first} {mid} {after}");
        validate(&mid).unwrap();
    }

    #[test]
    fn two_keys_inserted_at_the_same_place_can_be_split_again() {
        let a = key_between_with(None, None, &mut Fixed('a')).unwrap();
        let b = key_between_with(None, None, &mut Fixed('b')).unwrap();
        assert!(a < b);
        let mid = key_between_with(Some(&a), Some(&b), &mut Fixed('z')).unwrap();
        assert!(a < mid && mid < b, "{a} {mid} {b}");
    }

    #[test]
    fn many_inserts_keep_order() {
        let mut keys: Vec<String> = vec![key_between_random(None, None).unwrap()];
        for i in 0..300 {
            // 轮流插在最前、最后、中间。
            let k = match i % 3 {
                0 => key_between_random(None, Some(&keys[0])).unwrap(),
                1 => key_between_random(keys.last().map(String::as_str), None).unwrap(),
                _ => {
                    let m = keys.len() / 2;
                    key_between_random(Some(&keys[m - 1]), Some(&keys[m])).unwrap()
                }
            };
            validate(&k).unwrap();
            keys.push(k);
            keys.sort();
            keys.dedup();
        }
        assert_eq!(keys.len(), 301);
    }
}
