//! 上下文仓库根目录的 `teams.yaml`：团队、岗位、项目的定义（`docs/spec.md` 第 10 节）。
//!
//! ```yaml
//! teams:
//!   research:
//!     roles: [macro, data]
//! projects: [q4-report]
//! ```

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::model::{Scope, valid_name};

pub const TEAMS_FILE: &str = "teams.yaml";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Teams {
    #[serde(default)]
    pub teams: BTreeMap<String, TeamDef>,
    #[serde(default)]
    pub projects: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamDef {
    #[serde(default)]
    pub roles: Vec<String>,
}

impl Teams {
    pub fn parse(text: &str) -> Result<Self, String> {
        let teams: Teams = if text.trim().is_empty() {
            Teams::default()
        } else {
            serde_saphyr::from_str(text).map_err(|e| format!("{TEAMS_FILE} 解析不了：{e}"))?
        };
        let mut roles = BTreeMap::new();
        for (team, def) in &teams.teams {
            if !valid_name(team) {
                return Err(format!("团队名不对：{team}"));
            }
            for role in &def.roles {
                if !valid_name(role) {
                    return Err(format!("岗位名不对：{role}"));
                }
                if let Some(other) = roles.insert(role.as_str(), team.as_str()) {
                    return Err(format!("岗位 {role} 同时属于 {other} 和 {team}"));
                }
            }
        }
        if let Some(p) = teams.projects.iter().find(|p| !valid_name(p)) {
            return Err(format!("项目名不对：{p}"));
        }
        Ok(teams)
    }

    /// 岗位属于哪个团队。
    pub fn team_of_role(&self, role: &str) -> Option<&str> {
        self.teams
            .iter()
            .find(|(_, def)| def.roles.iter().any(|r| r == role))
            .map(|(team, _)| team.as_str())
    }

    /// 这个范围在 `teams.yaml` 里有没有定义。`org` 总是有。
    pub fn defines(&self, scope: &Scope) -> bool {
        match scope {
            Scope::Org => true,
            Scope::Team(t) => self.teams.contains_key(t),
            Scope::Role(r) => self.team_of_role(r).is_some(),
            Scope::Project(p) => self.projects.iter().any(|x| x == p),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_lookup() {
        let t =
            Teams::parse("teams:\n  research:\n    roles: [macro, data]\nprojects: [q4-report]\n")
                .unwrap();
        assert_eq!(t.team_of_role("macro"), Some("research"));
        assert!(t.defines(&Scope::Org));
        assert!(t.defines(&Scope::Project("q4-report".into())));
        assert!(!t.defines(&Scope::Team("ops".into())));
        assert_eq!(Teams::parse("").unwrap(), Teams::default());
    }

    #[test]
    fn rejects_bad_files() {
        assert!(Teams::parse("teams:\n  a:\n    roles: [x]\n  b:\n    roles: [x]\n").is_err());
        assert!(Teams::parse("people: [me]\n").is_err());
        assert!(Teams::parse("teams:\n  \"a b\": {}\n").is_err());
    }
}
