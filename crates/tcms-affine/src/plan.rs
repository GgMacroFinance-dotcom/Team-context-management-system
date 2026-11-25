//! 一篇文档的三方比较（`docs/affine-sync.md` 5.2–5.4）：git 里现在的样子、AFFiNE 里现在的样子、
//! 上次同步完两边一致的样子（底稿），算出这次哪些条目写到哪一边、哪些两边都改了要人处理。
//!
//! 只比标题和正文。条目的属性（类型、标签、负责人、复核日期）不进 AFFiNE，写回 git 时原样留着。
//! 正文按块比较（[`affine_doc::same_content`]）：Markdown 写法不一样、内容一样（`*` 和 `-` 的列表）不算改了。
//! 这里不碰网络、数据库和 git，只算。

use std::collections::{HashMap, HashSet};

use affine_doc::{Block, same_content, to_blocks};

/// git 里的一条（文件里的顺序）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitItem {
    pub id: String,
    pub title: String,
    pub body: String,
    /// 正文里有 AFFiNE 表示不全的写法（表格、图片、HTML……）：写进 AFFiNE 只能看，那边改了不往回写。
    pub readonly: bool,
}

impl GitItem {
    pub fn new(id: impl Into<String>, title: impl Into<String>, body: impl Into<String>) -> Self {
        let body = body.into();
        Self {
            id: id.into(),
            title: title.into(),
            readonly: readonly(&body),
            body,
        }
    }
}

/// 这段正文有没有 AFFiNE 表示不全的写法（表格、图片、HTML……）：有的话写进 AFFiNE 只能看，那边改了不往回写。
pub fn readonly(body: &str) -> bool {
    to_blocks(body).iter().any(Block::is_lossy)
}

/// AFFiNE 里的一条（文档里的顺序）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AffineItem {
    pub heading_id: String,
    pub title: String,
    pub blocks: Vec<Block>,
    /// 正文的 Markdown（[`affine_doc::to_markdown`]）。
    pub body: String,
}

/// 上次同步完的样子。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Base {
    pub item_id: String,
    pub heading_id: String,
    pub title: String,
    pub body: String,
}

/// 写到 git 的。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToGit {
    /// 换掉这一条的标题和正文（属性不动）。
    Replace {
        id: String,
        title: String,
        body: String,
    },
    /// AFFiNE 里删了：归档。
    Remove { id: String },
    /// AFFiNE 里新加的一条，放在 `after` 那一条后面（`None` 时放在文件最后）。
    Insert {
        id: String,
        after: Option<String>,
        heading_id: String,
        title: String,
        body: String,
    },
}

/// 写到 AFFiNE 的。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToAffine {
    Update {
        heading_id: String,
        title: String,
        body: String,
    },
    Remove {
        heading_id: String,
    },
    /// git 里新加的一条，放在 `after` 那一条（条目 id）后面（`None` 时放在正文最后）。
    Insert {
        item_id: String,
        after: Option<String>,
        title: String,
        body: String,
    },
}

/// 为什么没写过去。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    /// 两边都改了，改得不一样。
    Both,
    /// AFFiNE 里删了，git 里改了。
    AffineDeleted,
    /// git 里删了（归档、取代、挪走），AFFiNE 里改了。
    GitDeleted,
    /// git 里的写法 AFFiNE 表示不全，AFFiNE 里改了不往回写。
    Readonly,
    /// 写不过去（比如写进 Markdown 以后读回来对不上）。
    Invalid,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Both => "both",
            Reason::AffineDeleted => "affine_deleted",
            Reason::GitDeleted => "git_deleted",
            Reason::Readonly => "readonly",
            Reason::Invalid => "invalid",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            Reason::Both,
            Reason::AffineDeleted,
            Reason::GitDeleted,
            Reason::Readonly,
            Reason::Invalid,
        ]
        .into_iter()
        .find(|r| r.as_str() == s)
    }

    /// 给人看的一句话。
    pub fn label(self) -> &'static str {
        match self {
            Reason::Both => "两边都改了，改得不一样",
            Reason::AffineDeleted => "AFFiNE 里删了，git 里改了",
            Reason::GitDeleted => "git 里删了，AFFiNE 里改了",
            Reason::Readonly => {
                "git 里有 AFFiNE 表示不全的写法（表格、图片……），AFFiNE 里改了不往回写"
            }
            Reason::Invalid => "写不过去",
        }
    }
}

/// 一边的标题和正文。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Text {
    pub title: String,
    pub body: String,
}

/// 记下来等人处理的一条。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    pub reason: Reason,
    pub item_id: Option<String>,
    pub heading_id: Option<String>,
    pub git: Option<Text>,
    pub affine: Option<Text>,
    pub base: Option<Text>,
    pub message: Option<String>,
}

/// 底稿怎么改。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaseChange {
    Set(Base),
    Remove { item_id: String },
}

/// 算出来的这次要做的事。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub to_git: Vec<ToGit>,
    pub to_affine: Vec<ToAffine>,
    /// 写进 AFFiNE 的新条目的底稿要等写完、知道标题块 id 以后再记，不在这里。
    pub bases: Vec<BaseChange>,
    pub held: Vec<Held>,
}

impl Plan {
    /// 因为 AFFiNE 里删了、要在 git 里归档的条数。
    pub fn archived(&self) -> usize {
        self.to_git
            .iter()
            .filter(|op| matches!(op, ToGit::Remove { .. }))
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.to_git.is_empty()
            && self.to_affine.is_empty()
            && self.bases.is_empty()
            && self.held.is_empty()
    }
}

/// 现在等人处理的（这些条目两边都不动）。
#[derive(Clone, Debug, Default)]
pub struct Pending {
    pub item_ids: HashSet<String>,
    pub heading_ids: HashSet<String>,
}

/// 两段 Markdown 内容一样。
pub fn same_markdown(a: &str, b: &str) -> bool {
    a == b || same_content(&to_blocks(a), &to_blocks(b))
}

fn git_text(g: &GitItem) -> Text {
    Text {
        title: g.title.clone(),
        body: g.body.clone(),
    }
}

fn affine_text(a: &AffineItem) -> Text {
    Text {
        title: a.title.clone(),
        body: a.body.clone(),
    }
}

fn base_text(b: &Base) -> Text {
    Text {
        title: b.title.clone(),
        body: b.body.clone(),
    }
}

/// 算一篇文档。`git`、`affine` 按各自的顺序；`bases` 是这篇文档的底稿；`new_id` 给 AFFiNE 里新加的条目发 id。
/// AFFiNE 里标题是空的二级标题还不算一条（人刚敲了 `##`）：对应上的那条这次不动，没对应上的不管。
pub fn plan(
    git: &[GitItem],
    affine: &[AffineItem],
    bases: &[Base],
    pending: &Pending,
    new_id: &mut dyn FnMut() -> String,
) -> Plan {
    let mut out = Plan::default();
    let git_by_id: HashMap<&str, &GitItem> = git.iter().map(|g| (g.id.as_str(), g)).collect();
    let aff_by_heading: HashMap<&str, &AffineItem> =
        affine.iter().map(|a| (a.heading_id.as_str(), a)).collect();
    let base_by_item: HashMap<&str, &Base> =
        bases.iter().map(|b| (b.item_id.as_str(), b)).collect();
    let base_by_heading: HashMap<&str, &Base> =
        bases.iter().map(|b| (b.heading_id.as_str(), b)).collect();

    // 这次以后 git 里还在的条目（AFFiNE 里新加的条目按标题块记）。
    let mut gone_from_git: HashSet<&str> = HashSet::new();
    let mut gone_from_affine: HashSet<&str> = HashSet::new();

    for b in bases {
        let held =
            pending.item_ids.contains(&b.item_id) || pending.heading_ids.contains(&b.heading_id);
        let g = git_by_id.get(b.item_id.as_str()).copied();
        let a = aff_by_heading.get(b.heading_id.as_str()).copied();
        if held {
            continue;
        }
        if a.is_some_and(|a| a.title.is_empty()) {
            continue;
        }
        let git_changed = |g: &GitItem| !(g.title == b.title && same_markdown(&g.body, &b.body));
        let affine_changed =
            |a: &AffineItem| !(a.title == b.title && same_content(&a.blocks, &to_blocks(&b.body)));
        match (g, a) {
            (Some(g), Some(a)) => {
                let (gc, ac) = (git_changed(g), affine_changed(a));
                match (gc, ac) {
                    (false, false) => {}
                    (true, false) => {
                        out.to_affine.push(ToAffine::Update {
                            heading_id: b.heading_id.clone(),
                            title: g.title.clone(),
                            body: g.body.clone(),
                        });
                        out.bases.push(BaseChange::Set(Base {
                            title: g.title.clone(),
                            body: g.body.clone(),
                            ..b.clone()
                        }));
                    }
                    (false, true) if g.readonly => out.held.push(Held {
                        reason: Reason::Readonly,
                        item_id: Some(b.item_id.clone()),
                        heading_id: Some(b.heading_id.clone()),
                        git: Some(git_text(g)),
                        affine: Some(affine_text(a)),
                        base: Some(base_text(b)),
                        message: None,
                    }),
                    (false, true) => {
                        out.to_git.push(ToGit::Replace {
                            id: b.item_id.clone(),
                            title: a.title.clone(),
                            body: a.body.clone(),
                        });
                        out.bases.push(BaseChange::Set(Base {
                            title: a.title.clone(),
                            body: a.body.clone(),
                            ..b.clone()
                        }));
                    }
                    (true, true) => {
                        if g.title == a.title && same_content(&to_blocks(&g.body), &a.blocks) {
                            out.bases.push(BaseChange::Set(Base {
                                title: g.title.clone(),
                                body: g.body.clone(),
                                ..b.clone()
                            }));
                        } else {
                            out.held.push(Held {
                                reason: Reason::Both,
                                item_id: Some(b.item_id.clone()),
                                heading_id: Some(b.heading_id.clone()),
                                git: Some(git_text(g)),
                                affine: Some(affine_text(a)),
                                base: Some(base_text(b)),
                                message: None,
                            });
                        }
                    }
                }
            }
            (Some(g), None) => {
                if git_changed(g) {
                    out.held.push(Held {
                        reason: Reason::AffineDeleted,
                        item_id: Some(b.item_id.clone()),
                        heading_id: Some(b.heading_id.clone()),
                        git: Some(git_text(g)),
                        affine: None,
                        base: Some(base_text(b)),
                        message: None,
                    });
                } else {
                    gone_from_git.insert(b.item_id.as_str());
                    out.to_git.push(ToGit::Remove {
                        id: b.item_id.clone(),
                    });
                    out.bases.push(BaseChange::Remove {
                        item_id: b.item_id.clone(),
                    });
                }
            }
            (None, Some(a)) => {
                if affine_changed(a) {
                    out.held.push(Held {
                        reason: Reason::GitDeleted,
                        item_id: Some(b.item_id.clone()),
                        heading_id: Some(b.heading_id.clone()),
                        git: None,
                        affine: Some(affine_text(a)),
                        base: Some(base_text(b)),
                        message: None,
                    });
                } else {
                    gone_from_affine.insert(b.heading_id.as_str());
                    out.to_affine.push(ToAffine::Remove {
                        heading_id: b.heading_id.clone(),
                    });
                    out.bases.push(BaseChange::Remove {
                        item_id: b.item_id.clone(),
                    });
                }
            }
            (None, None) => out.bases.push(BaseChange::Remove {
                item_id: b.item_id.clone(),
            }),
        }
    }

    // AFFiNE 里新加的：放在 AFFiNE 里它前面那一条（这次以后 git 里还在的）后面。
    let mut prev: Option<String> = None;
    for a in affine {
        if let Some(b) = base_by_heading.get(a.heading_id.as_str()) {
            if git_by_id.contains_key(b.item_id.as_str())
                && !gone_from_git.contains(b.item_id.as_str())
            {
                prev = Some(b.item_id.clone());
            }
            continue;
        }
        if a.title.is_empty() || pending.heading_ids.contains(&a.heading_id) {
            continue;
        }
        let id = new_id();
        out.to_git.push(ToGit::Insert {
            id: id.clone(),
            after: prev.clone(),
            heading_id: a.heading_id.clone(),
            title: a.title.clone(),
            body: a.body.clone(),
        });
        out.bases.push(BaseChange::Set(Base {
            item_id: id.clone(),
            heading_id: a.heading_id.clone(),
            title: a.title.clone(),
            body: a.body.clone(),
        }));
        prev = Some(id);
    }

    // git 里新加的：放在 git 里它前面那一条（这次以后 AFFiNE 里还在的）后面。
    let mut prev: Option<String> = None;
    for g in git {
        if let Some(b) = base_by_item.get(g.id.as_str()) {
            if aff_by_heading.contains_key(b.heading_id.as_str())
                && !gone_from_affine.contains(b.heading_id.as_str())
            {
                prev = Some(g.id.clone());
            }
            continue;
        }
        if pending.item_ids.contains(&g.id) {
            continue;
        }
        out.to_affine.push(ToAffine::Insert {
            item_id: g.id.clone(),
            after: prev.clone(),
            title: g.title.clone(),
            body: g.body.clone(),
        });
        prev = Some(g.id.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aff(heading: &str, title: &str, body: &str) -> AffineItem {
        AffineItem {
            heading_id: heading.to_owned(),
            title: title.to_owned(),
            blocks: to_blocks(body),
            body: body.to_owned(),
        }
    }

    fn base(item: &str, heading: &str, title: &str, body: &str) -> Base {
        Base {
            item_id: item.to_owned(),
            heading_id: heading.to_owned(),
            title: title.to_owned(),
            body: body.to_owned(),
        }
    }

    fn ids() -> impl FnMut() -> String {
        let mut n = 0;
        move || {
            n += 1;
            format!("new{n}")
        }
    }

    fn run(git: &[GitItem], affine: &[AffineItem], bases: &[Base]) -> Plan {
        plan(git, affine, bases, &Pending::default(), &mut ids())
    }

    #[test]
    fn nothing_changed() {
        let g = [GitItem::new("a", "甲", "- 一\n- 二")];
        // 写法不一样、内容一样不算改了。
        let a = [aff("h1", "甲", "* 一\n* 二")];
        let b = [base("a", "h1", "甲", "- 一\n- 二")];
        assert!(run(&g, &a, &b).is_empty());
    }

    #[test]
    fn one_side_changed() {
        let b = [base("a", "h1", "甲", "原来的")];
        let p = run(
            &[GitItem::new("a", "甲", "git 改的")],
            &[aff("h1", "甲", "原来的")],
            &b,
        );
        assert_eq!(
            p.to_affine,
            vec![ToAffine::Update {
                heading_id: "h1".into(),
                title: "甲".into(),
                body: "git 改的".into()
            }]
        );
        assert!(p.to_git.is_empty());
        assert_eq!(
            p.bases,
            vec![BaseChange::Set(base("a", "h1", "甲", "git 改的"))]
        );

        let p = run(
            &[GitItem::new("a", "甲", "原来的")],
            &[aff("h1", "甲乙", "**AFFiNE** 改的")],
            &b,
        );
        assert_eq!(
            p.to_git,
            vec![ToGit::Replace {
                id: "a".into(),
                title: "甲乙".into(),
                body: "**AFFiNE** 改的".into()
            }]
        );
        assert!(p.to_affine.is_empty());
    }

    #[test]
    fn both_changed() {
        let b = [base("a", "h1", "甲", "原来的")];
        // 改成一样的：只更新底稿。
        let p = run(
            &[GitItem::new("a", "甲", "一样")],
            &[aff("h1", "甲", "一样")],
            &b,
        );
        assert!(p.to_git.is_empty() && p.to_affine.is_empty() && p.held.is_empty());
        assert_eq!(
            p.bases,
            vec![BaseChange::Set(base("a", "h1", "甲", "一样"))]
        );
        // 改得不一样：记下来，两边都不动。
        let p = run(
            &[GitItem::new("a", "甲", "git 的")],
            &[aff("h1", "甲", "AFFiNE 的")],
            &b,
        );
        assert!(p.to_git.is_empty() && p.to_affine.is_empty() && p.bases.is_empty());
        assert_eq!(p.held.len(), 1);
        assert_eq!(p.held[0].reason, Reason::Both);
        assert_eq!(p.held[0].git.as_ref().unwrap().body, "git 的");
        assert_eq!(p.held[0].affine.as_ref().unwrap().body, "AFFiNE 的");
        assert_eq!(p.held[0].base.as_ref().unwrap().body, "原来的");
    }

    #[test]
    fn deletions() {
        let b = [base("a", "h1", "甲", "一"), base("b", "h2", "乙", "二")];
        // AFFiNE 里删了乙：git 里归档。git 里删了甲：AFFiNE 里删掉。
        let p = run(
            &[GitItem::new("b", "乙", "二")],
            &[aff("h1", "甲", "一")],
            &b,
        );
        assert_eq!(p.to_git, vec![ToGit::Remove { id: "b".into() }]);
        assert_eq!(
            p.to_affine,
            vec![ToAffine::Remove {
                heading_id: "h1".into()
            }]
        );
        assert_eq!(p.archived(), 1);
        assert_eq!(p.bases.len(), 2);
        // 删的那边另一边改过：记下来。
        let p = run(
            &[GitItem::new("b", "乙", "git 改了")],
            &[aff("h1", "甲", "AFFiNE 改了")],
            &b,
        );
        assert!(p.to_git.is_empty() && p.to_affine.is_empty());
        let reasons: Vec<_> = p.held.iter().map(|h| h.reason).collect();
        assert_eq!(reasons, vec![Reason::GitDeleted, Reason::AffineDeleted]);
        // 两边都删了：只去掉底稿。
        let p = run(&[], &[], &b[..1]);
        assert_eq!(
            p.bases,
            vec![BaseChange::Remove {
                item_id: "a".into()
            }]
        );
    }

    #[test]
    fn new_items_keep_their_place() {
        let b = [base("a", "h1", "甲", "一"), base("c", "h3", "丙", "三")];
        let git = [
            GitItem::new("a", "甲", "一"),
            GitItem::new("g", "git 新加的", "正文"),
            GitItem::new("c", "丙", "三"),
        ];
        let affine = [
            aff("hx", "AFFiNE 最前面加的", "x"),
            aff("h1", "甲", "一"),
            aff("h3", "丙", "三"),
            aff("hy", "AFFiNE 后面加的", "y"),
            aff("hz", "", "还没写标题"),
        ];
        let p = run(&git, &affine, &b);
        assert_eq!(
            p.to_git,
            vec![
                ToGit::Insert {
                    id: "new1".into(),
                    after: None,
                    heading_id: "hx".into(),
                    title: "AFFiNE 最前面加的".into(),
                    body: "x".into()
                },
                ToGit::Insert {
                    id: "new2".into(),
                    after: Some("c".into()),
                    heading_id: "hy".into(),
                    title: "AFFiNE 后面加的".into(),
                    body: "y".into()
                },
            ]
        );
        assert_eq!(
            p.to_affine,
            vec![ToAffine::Insert {
                item_id: "g".into(),
                after: Some("a".into()),
                title: "git 新加的".into(),
                body: "正文".into()
            }]
        );
        assert_eq!(p.bases.len(), 2);
    }

    #[test]
    fn readonly_items_are_not_written_back() {
        let table = "| a | b |\n| - | - |\n| 1 | 2 |";
        let g = GitItem::new("a", "表", table);
        assert!(g.readonly);
        let b = [base("a", "h1", "表", table)];
        let p = run(&[g], &[aff("h1", "表", "AFFiNE 里改了")], &b);
        assert!(p.to_git.is_empty());
        assert_eq!(p.held[0].reason, Reason::Readonly);
    }

    #[test]
    fn pending_and_untitled_items_are_left_alone() {
        let b = [base("a", "h1", "甲", "一"), base("b", "h2", "乙", "二")];
        let pending = Pending {
            item_ids: ["a".to_owned()].into(),
            heading_ids: ["hn".to_owned()].into(),
        };
        let git = [
            GitItem::new("a", "甲", "git 改了"),
            GitItem::new("b", "乙", "二"),
        ];
        let affine = [
            aff("h1", "甲", "AFFiNE 改了"),
            aff("h2", "", "标题刚删掉"),
            aff("hn", "等人处理的新条目", "x"),
        ];
        assert!(plan(&git, &affine, &b, &pending, &mut ids()).is_empty());
    }
}
