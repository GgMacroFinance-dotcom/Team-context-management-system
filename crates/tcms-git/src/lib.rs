//! 上下文仓库的 git 读写，用 gix（纯 Rust）。见 `docs/architecture.md` 第 3、4 节。
//!
//! 读：某个提交里有哪些文件、两个提交之间哪些文件变了、读文件内容。
//! 写：把一组文件改动做成一个提交，同时更新工作区和暂存区。服务是唯一的写入方，
//! 写之前要求工作区是干净的，免得覆盖人还没提交的改动。
//! 远端：拉取、推送（[`Remote`]，调 git 命令行）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gix::bstr::ByteSlice;
use gix::objs::tree::EntryKind;

pub use gix::ObjectId;

mod remote;
pub use remote::{Pulled, Pushed, Remote, RemoteError, redact};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("git：{0}")]
    Git(#[from] gix::Error),
    #[error("提交 {0} 不在仓库里")]
    MissingCommit(ObjectId),
    #[error("仓库没有工作区（是裸仓库），不能写入")]
    Bare,
    #[error("工作区有没提交的改动，先提交或撤销再写入")]
    Dirty,
    #[error("{0} 已经存在（没有被 git 跟踪），不覆盖")]
    UntrackedExists(String),
    #[error("路径不对：{0}")]
    BadPath(String),
    #[error("HEAD 已经变了（别人刚提交过），重新读一遍再改")]
    HeadMoved,
    #[error("读写文件 {path}：{source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// 提交者。
#[derive(Debug, Clone)]
pub struct Author {
    pub name: String,
    pub email: String,
}

impl Default for Author {
    fn default() -> Self {
        Self {
            name: "tcms".to_owned(),
            email: "tcms@localhost".to_owned(),
        }
    }
}

/// 一个文件的改动。`content` 为空表示删除。
#[derive(Debug, Clone)]
pub struct FileEdit {
    pub path: String,
    pub content: Option<Vec<u8>>,
}

/// 两个提交之间一个文件的变化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// 新增或修改。
    Upserted(String),
    Deleted(String),
}

pub struct ContextRepo {
    repo: gix::Repository,
}

impl ContextRepo {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            repo: gix::open(path)?,
        })
    }

    /// 新建一个空仓库（测试和基准用）。
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            repo: gix::init(path)?,
        })
    }

    pub fn workdir(&self) -> Option<&Path> {
        self.repo.workdir()
    }

    /// 当前 HEAD 指向的提交；还没有提交时为空。
    pub fn head(&self) -> Result<Option<ObjectId>> {
        let mut head = self.repo.head()?;
        Ok(head.try_peel_to_id()?.map(|id| id.detach()))
    }

    pub fn has_commit(&self, id: ObjectId) -> bool {
        self.repo.find_commit(id).is_ok()
    }

    /// 某个提交里的所有文件：路径到内容的对象 id。路径用 `/` 分隔，按路径排序。
    pub fn files(&self, commit: ObjectId) -> Result<BTreeMap<String, ObjectId>> {
        let commit = self
            .repo
            .find_commit(commit)
            .map_err(|_| Error::MissingCommit(commit))?;
        let tree = commit.tree()?;
        let mut out = BTreeMap::new();
        for entry in tree.traverse().breadthfirst.files()? {
            if entry.mode.is_blob() {
                let path = entry.filepath.to_str_lossy().into_owned();
                out.insert(path, entry.oid);
            }
        }
        Ok(out)
    }

    pub fn read_blob(&self, id: ObjectId) -> Result<Vec<u8>> {
        Ok(self.repo.find_blob(id)?.data.clone())
    }

    /// 从 `from` 到 `to` 变了哪些文件。`from` 为空表示从空仓库算起。
    pub fn changes(&self, from: Option<ObjectId>, to: ObjectId) -> Result<Vec<Change>> {
        let old = match from {
            Some(id) => self.files(id)?,
            None => BTreeMap::new(),
        };
        let new = self.files(to)?;
        Ok(diff_files(&old, &new))
    }

    /// 把一组改动做成一个提交，HEAD 往前走一步，工作区和暂存区跟着更新。
    pub fn commit(&self, edits: &[FileEdit], message: &str, author: &Author) -> Result<ObjectId> {
        let parent = self.head()?;
        self.commit_on(parent, edits, message, author)
    }

    /// 和 [`ContextRepo::commit`] 一样，但要求 HEAD 还是 `parent`（改动是照着 `parent` 算的）。
    /// HEAD 变了返回 [`Error::HeadMoved`]，不提交。
    pub fn commit_on(
        &self,
        parent: Option<ObjectId>,
        edits: &[FileEdit],
        message: &str,
        author: &Author,
    ) -> Result<ObjectId> {
        let workdir = self.repo.workdir().ok_or(Error::Bare)?.to_owned();
        if self.head()? != parent {
            return Err(Error::HeadMoved);
        }
        if self.repo.is_dirty()? {
            return Err(Error::Dirty);
        }
        let base_files = match parent {
            Some(p) => self.files(p)?,
            None => BTreeMap::new(),
        };
        for edit in edits {
            check_path(&edit.path)?;
            if edit.content.is_some()
                && !base_files.contains_key(&edit.path)
                && workdir.join(&edit.path).exists()
            {
                return Err(Error::UntrackedExists(edit.path.clone()));
            }
        }

        let base_tree = self.repo.head_tree_id_or_empty()?.detach();
        let mut editor = self.repo.edit_tree(base_tree)?;
        for edit in edits {
            match &edit.content {
                Some(bytes) => {
                    let blob = self.repo.write_blob(bytes)?.detach();
                    editor.upsert(edit.path.as_str(), EntryKind::Blob, blob)?;
                }
                None => {
                    editor.remove(edit.path.as_str())?;
                }
            }
        }
        let tree = editor.write()?.detach();

        let time = gix::date::Time::now_local_or_utc();
        let mut buf = gix::date::parse::TimeBuf::default();
        let sig = gix::actor::Signature {
            name: author.name.as_str().into(),
            email: author.email.as_str().into(),
            time,
        };
        let sig_ref = sig.to_ref(&mut buf);
        let id = self
            .repo
            .commit_as(sig_ref, sig_ref, "HEAD", message, tree, parent)?
            .detach();

        // 工作区：写新内容、删掉删除的文件。
        for edit in edits {
            let path = workdir.join(&edit.path);
            let io = |source| Error::Io {
                path: path.clone(),
                source,
            };
            match &edit.content {
                Some(bytes) => {
                    if let Some(dir) = path.parent() {
                        std::fs::create_dir_all(dir).map_err(io)?;
                    }
                    std::fs::write(&path, bytes).map_err(io)?;
                }
                None => match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io(e)),
                },
            }
        }
        // 暂存区：照新提交的树重建。工作区原来是干净的，这样不会丢东西。
        let mut index = self.repo.index_from_tree(&tree)?;
        index.write(Default::default())?;
        Ok(id)
    }
}

/// 只允许普通的相对路径：不能是绝对路径，不能有 `.`、`..`、空段，不能碰 `.git`。
fn check_path(path: &str) -> Result<()> {
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.split('/').any(|seg| {
            seg.is_empty() || seg == "." || seg == ".." || seg.eq_ignore_ascii_case(".git")
        });
    if bad {
        Err(Error::BadPath(path.to_owned()))
    } else {
        Ok(())
    }
}

fn diff_files(old: &BTreeMap<String, ObjectId>, new: &BTreeMap<String, ObjectId>) -> Vec<Change> {
    let mut out = Vec::new();
    for (path, id) in new {
        if old.get(path) != Some(id) {
            out.push(Change::Upserted(path.clone()));
        }
    }
    for path in old.keys() {
        if !new.contains_key(path) {
            out.push(Change::Deleted(path.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(path: &str, content: &str) -> FileEdit {
        FileEdit {
            path: path.to_owned(),
            content: Some(content.as_bytes().to_vec()),
        }
    }

    #[test]
    fn commit_read_and_diff() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ContextRepo::init(dir.path()).unwrap();
        assert_eq!(repo.head().unwrap(), None);
        let author = Author::default();

        let c1 = repo
            .commit(
                &[edit("org/a.md", "一\n"), edit("team-x/b.md", "二\n")],
                "第一个",
                &author,
            )
            .unwrap();
        assert_eq!(repo.head().unwrap(), Some(c1));
        let files = repo.files(c1).unwrap();
        assert_eq!(
            files.keys().collect::<Vec<_>>(),
            ["org/a.md", "team-x/b.md"]
        );
        assert_eq!(
            repo.read_blob(files["org/a.md"]).unwrap(),
            b"\xe4\xb8\x80\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("team-x/b.md")).unwrap(),
            "二\n"
        );

        let c2 = repo
            .commit(
                &[
                    edit("org/a.md", "改了\n"),
                    FileEdit {
                        path: "team-x/b.md".into(),
                        content: None,
                    },
                    edit("org/c.md", "三\n"),
                ],
                "第二个",
                &author,
            )
            .unwrap();
        assert!(!dir.path().join("team-x/b.md").exists());
        let mut changes = repo.changes(Some(c1), c2).unwrap();
        changes.sort_by_key(|c| format!("{c:?}"));
        assert_eq!(
            changes,
            [
                Change::Deleted("team-x/b.md".into()),
                Change::Upserted("org/a.md".into()),
                Change::Upserted("org/c.md".into()),
            ]
        );
        assert_eq!(repo.changes(None, c1).unwrap().len(), 2);
        assert!(repo.has_commit(c1));
        // 写完以后工作区是干净的，可以接着写。
        repo.commit(&[edit("org/d.md", "四\n")], "第三个", &author)
            .unwrap();
    }

    #[test]
    fn refuses_dirty_worktree_and_untracked_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ContextRepo::init(dir.path()).unwrap();
        let author = Author::default();
        repo.commit(&[edit("org/a.md", "一\n")], "第一个", &author)
            .unwrap();

        std::fs::write(dir.path().join("org/new.md"), "人放的\n").unwrap();
        assert!(matches!(
            repo.commit(&[edit("org/new.md", "覆盖\n")], "x", &author),
            Err(Error::UntrackedExists(_))
        ));

        std::fs::write(dir.path().join("org/a.md"), "人改了还没提交\n").unwrap();
        assert!(matches!(
            repo.commit(&[edit("org/b.md", "x\n")], "x", &author),
            Err(Error::Dirty)
        ));
    }

    #[test]
    fn rejects_bad_paths() {
        for p in ["", "/abs", "a/../b", "a//b", ".git/config", "a\\b", "./a"] {
            assert!(check_path(p).is_err(), "{p}");
        }
        assert!(check_path("org/子目录/a.md").is_ok());
    }

    /// 如果机器上有 git 命令行，确认它也认为写完以后工作区是干净的。
    #[test]
    fn git_cli_sees_clean_worktree() {
        let Ok(out) = std::process::Command::new("git").arg("--version").output() else {
            return;
        };
        if !out.status.success() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = ContextRepo::init(dir.path()).unwrap();
        let author = Author::default();
        repo.commit(&[edit("org/a.md", "一\n")], "第一个", &author)
            .unwrap();
        repo.commit(
            &[edit("org/a.md", "二\n"), edit("org/b/c.md", "三\n")],
            "第二个",
            &author,
        )
        .unwrap();
        let status = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(status.status.success());
        assert_eq!(String::from_utf8_lossy(&status.stdout), "");
        let log = std::process::Command::new("git")
            .args(["log", "--format=%an <%ae> %s"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&log.stdout),
            "tcms <tcms@localhost> 第二个\ntcms <tcms@localhost> 第一个\n"
        );
    }
}
