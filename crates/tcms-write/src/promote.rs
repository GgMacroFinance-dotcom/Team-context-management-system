//! 晋升（`docs/spec.md` 第 5 节）：把一条岗位、团队、项目的条目提到更大的范围。
//!
//! 晋升就是一条换了范围的取代提议：在更大的范围里写一条一样的（标题、正文、类型、标签、复核日期照抄），
//! 取代原来那条。人在审核页或命令行发起，进待审核的列表，要再批准一次才生效，不自动晋升。
//! agent 用 `context_propose` 的 `supersede` 本来就能往自己能写的范围里提。
//!
//! 能提到哪里：岗位 → 它在 `teams.yaml` 里所在的团队，或者全公司；团队、项目 → 全公司。

use tcms_core::{Scope, TEAMS_FILE, Teams, redact};
use tcms_git::{ContextRepo, ObjectId};
use tcms_search::SearchIndex;
use tcms_store::{Action, Identity, NewProposal, Store};

use crate::conflicts::find_conflicts;
use crate::propose::{
    ContentKey, MAX_REASON_CHARS, Proposal, ProposeError, chars, content_hash, invalid, save,
};

/// 这个范围的条目能提到哪些范围，近的在前。全公司的没有可提的。
pub fn promotion_targets(scope: &Scope, teams: Option<&Teams>) -> Vec<Scope> {
    match scope {
        Scope::Org => Vec::new(),
        Scope::Role(role) => teams
            .and_then(|t| t.team_of_role(role))
            .map(|t| Scope::Team(t.to_owned()))
            .into_iter()
            .chain([Scope::Org])
            .collect(),
        Scope::Team(_) | Scope::Project(_) => vec![Scope::Org],
    }
}

/// 上下文仓库在 `commit`（索引到的那个提交，十六进制）时的 `teams.yaml`；没有这个文件时为空。
pub fn read_teams(repo: &ContextRepo, commit: &str) -> Result<Option<Teams>, String> {
    let id = ObjectId::from_hex(commit.as_bytes())
        .map_err(|e| format!("提交号不对：{commit}（{e}）"))?;
    let files = repo.files(id).map_err(|e| e.to_string())?;
    let Some(blob) = files.get(TEAMS_FILE).copied() else {
        return Ok(None);
    };
    let bytes = repo.read_blob(blob).map_err(|e| e.to_string())?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{TEAMS_FILE} 不是 UTF-8"))?;
    Teams::parse(&text).map(Some)
}

/// 提一条晋升：`item_id` 提到 `to`。`teams` 是上下文仓库里的 `teams.yaml`（查岗位属于哪个团队），
/// `by` 是发起的人（审核的人的令牌，或命令行）。同样的晋升还没处理时返回原来那条。
pub async fn promote(
    store: &Store,
    index: &SearchIndex,
    item_id: &str,
    to: &Scope,
    reason: &str,
    teams: Option<&Teams>,
    by: &Identity,
) -> Result<Proposal, ProposeError> {
    let reason = reason.trim();
    if reason.is_empty() {
        return invalid("写一句为什么要提到更大的范围，审核的人要看");
    }
    if chars(reason) > MAX_REASON_CHARS {
        return invalid(format!("理由太长了，最多 {MAX_REASON_CHARS} 字"));
    }
    let Some(item) = index.corpus().get(item_id) else {
        return invalid(format!("没有这一条，或者它已经不是生效的了：{item_id}"));
    };
    let targets = promotion_targets(&item.scope, teams);
    if targets.is_empty() {
        return invalid(format!("{item_id} 已经是全公司的了，没有更大的范围"));
    }
    if !targets.contains(to) {
        let names: Vec<String> = targets.iter().map(ToString::to_string).collect();
        return invalid(format!(
            "{} 的条目只能提到 {}",
            item.scope,
            names.join("、")
        ));
    }
    let (reason, redacted) = redact(reason);
    let conflicts = find_conflicts(
        index.corpus(),
        to,
        item.kind,
        &item.title,
        &item.tags,
        Some(item_id),
    );
    let content_hash = content_hash(&ContentKey {
        action: Action::Supersede.as_str(),
        item_id: Some(item_id),
        scope: to.to_string(),
        kind: item.kind.as_str(),
        title: &item.title,
        body: &item.body,
        tags: &item.tags,
        review_by: item.review_by.as_deref(),
    });
    let new = NewProposal {
        action: Action::Supersede,
        item_id: Some(item_id.to_owned()),
        base_revision: Some(item.revision),
        scope: to.to_string(),
        kind: item.kind.to_string(),
        title: item.title.clone(),
        body: item.body.clone(),
        tags: item.tags.clone(),
        review_by: item.review_by.clone(),
        reason,
        evidence: Vec::new(),
        conflicts: serde_json::to_value(&conflicts).unwrap_or_default(),
        content_hash,
        redacted: i32::try_from(redacted).unwrap_or(i32::MAX),
    };
    save(store, by, &new).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        let teams = Teams::parse("teams:\n  research:\n    roles: [macro]\n").unwrap();
        let t = |s: &str, teams: Option<&Teams>| -> Vec<String> {
            promotion_targets(&s.parse().unwrap(), teams)
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        assert_eq!(t("role:macro", Some(&teams)), ["team:research", "org"]);
        // 不在 teams.yaml 里的岗位、没有 teams.yaml：只能提到全公司。
        assert_eq!(t("role:legal", Some(&teams)), ["org"]);
        assert_eq!(t("role:macro", None), ["org"]);
        assert_eq!(t("team:research", Some(&teams)), ["org"]);
        assert_eq!(t("project:q4", Some(&teams)), ["org"]);
        assert!(t("org", Some(&teams)).is_empty());
    }
}
