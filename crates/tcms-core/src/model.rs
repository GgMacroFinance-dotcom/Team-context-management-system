//! 条目模型：id、类型、范围、地位、状态。见 `docs/spec.md` 第 3 节。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// 条目的稳定 id：`ctx_` 加 26 位 ULID。改内容不换 id。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ItemId(String);

impl ItemId {
    pub const PREFIX: &'static str = "ctx_";

    /// 生成一个新 id。
    pub fn generate() -> Self {
        Self(format!("{}{}", Self::PREFIX, ulid::Ulid::generate()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ItemId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s
            .strip_prefix(Self::PREFIX)
            .ok_or_else(|| format!("id 要以 {} 开头：{s}", Self::PREFIX))?;
        // ULID 是 26 位 Crockford base32；只检查长度和字符，不要求一定是本系统生成的。
        let ok = rest.len() == 26
            && rest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b.is_ascii_uppercase() && !b"ILOU".contains(&b)));
        if ok {
            Ok(Self(s.to_owned()))
        } else {
            Err(format!("id 格式不对（应为 ctx_ 加 26 位大写 ULID）：{s}"))
        }
    }
}

impl TryFrom<String> for ItemId {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<ItemId> for String {
    fn from(value: ItemId) -> Self {
        value.0
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 条目类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Rule,
    Decision,
    Fact,
    Term,
    Procedure,
    Reference,
    Memory,
}

impl Kind {
    pub const ALL: [Kind; 7] = [
        Kind::Rule,
        Kind::Decision,
        Kind::Fact,
        Kind::Term,
        Kind::Procedure,
        Kind::Reference,
        Kind::Memory,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Rule => "rule",
            Kind::Decision => "decision",
            Kind::Fact => "fact",
            Kind::Term => "term",
            Kind::Procedure => "procedure",
            Kind::Reference => "reference",
            Kind::Memory => "memory",
        }
    }

    /// 地位，数字越小越高（`docs/spec.md` 3.3）：
    /// 1 组织规则，2 人写的决定、事实、术语、流程，3 资料，4 记忆。
    pub fn authority(self) -> i16 {
        match self {
            Kind::Rule => 1,
            Kind::Decision | Kind::Fact | Kind::Term | Kind::Procedure => 2,
            Kind::Reference => 3,
            Kind::Memory => 4,
        }
    }
}

impl FromStr for Kind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Kind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| format!("不认识的类型 {s}，可选：rule、decision、fact、term、procedure、reference、memory"))
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 范围（`docs/spec.md` 3.2）。四层，不做“个人”。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    Org,
    Team(String),
    Role(String),
    Project(String),
}

impl Scope {
    /// 上下文仓库里第一层目录的名字：`org`、`team-x`、`role-x`、`project-x`。
    pub fn from_dir(dir: &str) -> Option<Scope> {
        if dir == "org" {
            return Some(Scope::Org);
        }
        type Ctor = fn(String) -> Scope;
        const PREFIXES: [(&str, Ctor); 3] = [
            ("team-", Scope::Team),
            ("role-", Scope::Role),
            ("project-", Scope::Project),
        ];
        let (ctor, name) = PREFIXES
            .iter()
            .find_map(|(prefix, ctor)| dir.strip_prefix(prefix).map(|n| (ctor, n)))?;
        valid_name(name).then(|| ctor(name.to_owned()))
    }

    pub fn dir(&self) -> String {
        match self {
            Scope::Org => "org".to_owned(),
            Scope::Team(n) => format!("team-{n}"),
            Scope::Role(n) => format!("role-{n}"),
            Scope::Project(n) => format!("project-{n}"),
        }
    }
}

/// 范围名：非空，只含字母、数字（含中文）、`-`、`_`。
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

impl fmt::Display for Scope {
    /// 存库和引用时的写法：`org`、`team:x`、`role:x`、`project:x`。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Org => f.write_str("org"),
            Scope::Team(n) => write!(f, "team:{n}"),
            Scope::Role(n) => write!(f, "role:{n}"),
            Scope::Project(n) => write!(f, "project:{n}"),
        }
    }
}

impl FromStr for Scope {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parsed = match s.split_once(':') {
            None if s == "org" => Some(Scope::Org),
            Some(("team", n)) if valid_name(n) => Some(Scope::Team(n.to_owned())),
            Some(("role", n)) if valid_name(n) => Some(Scope::Role(n.to_owned())),
            Some(("project", n)) if valid_name(n) => Some(Scope::Project(n.to_owned())),
            _ => None,
        };
        parsed.ok_or_else(|| format!("范围写法不对：{s}（应为 org、team:x、role:x 或 project:x）"))
    }
}

impl Serialize for Scope {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Scope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// 条目状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Superseded,
    Archived,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Superseded => "superseded",
            Status::Archived => "archived",
        }
    }
}

impl FromStr for Status {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Status::Active),
            "superseded" => Ok(Status::Superseded),
            "archived" => Ok(Status::Archived),
            _ => Err(format!("不认识的状态 {s}")),
        }
    }
}

/// 检查 `YYYY-MM-DD` 是不是一个真实存在的日期。
pub fn valid_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let num = |r: std::ops::Range<usize>| s[r].parse::<u32>().ok();
    let (Some(y), Some(m), Some(d)) = (num(0..4), num(5..7), num(8..10)) else {
        return false;
    };
    if !s[0..4].bytes().all(|c| c.is_ascii_digit())
        || !s[5..7].bytes().all(|c| c.is_ascii_digit())
        || !s[8..10].bytes().all(|c| c.is_ascii_digit())
    {
        return false;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    y >= 1 && (1..=days).contains(&d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_id_roundtrip() {
        let id = ItemId::generate();
        assert!(id.as_str().starts_with("ctx_"));
        assert_eq!(id.as_str().parse::<ItemId>().unwrap(), id);
        assert!("ctx_short".parse::<ItemId>().is_err());
        assert!("abc_01JDX4M6Q8ZZZZZZZZZZZZZZZZ".parse::<ItemId>().is_err());
        assert!("ctx_01jdx4m6q8zzzzzzzzzzzzzzzz".parse::<ItemId>().is_err());
    }

    #[test]
    fn scope_dirs_and_strings() {
        assert_eq!(Scope::from_dir("org"), Some(Scope::Org));
        assert_eq!(
            Scope::from_dir("team-research"),
            Some(Scope::Team("research".into()))
        );
        assert_eq!(
            Scope::from_dir("role-宏观"),
            Some(Scope::Role("宏观".into()))
        );
        assert_eq!(
            Scope::from_dir("project-q4-report"),
            Some(Scope::Project("q4-report".into()))
        );
        assert_eq!(Scope::from_dir("team-"), None);
        assert_eq!(Scope::from_dir("people-x"), None);
        assert_eq!(Scope::from_dir("team-a b"), None);
        for s in ["org", "team:research", "role:macro", "project:x"] {
            assert_eq!(s.parse::<Scope>().unwrap().to_string(), s);
        }
        assert!("person:me".parse::<Scope>().is_err());
        assert_eq!(Scope::Team("a".into()).dir(), "team-a");
    }

    #[test]
    fn dates() {
        assert!(valid_date("2027-03-31"));
        assert!(valid_date("2028-02-29"));
        assert!(!valid_date("2027-02-29"));
        assert!(!valid_date("2027-13-01"));
        assert!(!valid_date("2027-1-01"));
        assert!(!valid_date("+027-01-01"));
    }

    #[test]
    fn authority_order() {
        assert!(Kind::Rule.authority() < Kind::Decision.authority());
        assert!(Kind::Fact.authority() < Kind::Reference.authority());
        assert!(Kind::Reference.authority() < Kind::Memory.authority());
    }
}
