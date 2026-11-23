//! 上下文仓库和远端（客户自己的 GitHub、Gitea 等）之间的拉取和推送。见 `docs/architecture.md` 第 3 节“远端”。
//!
//! gix 没有推送，合并也得自己拼，所以这里调 git 命令行（容器镜像里有 git）。规则：
//!
//! - 拉：`git fetch` 远端的那个分支。本地落后就快进；两边都有新提交就合并，做一个合并提交；合并有冲突就放弃合并、
//!   什么都不改，把本地 HEAD 推到远端的旁支 `tcms/<分支>-unmerged`（只有 tcms 写它，可以覆盖），
//!   返回 [`RemoteError::NeedsMerge`]，等人在自己的克隆里合并好推回那个分支，下次拉的时候快进过去。
//! - 推：把本地 HEAD 推到远端的那个分支，不强推。远端有本地没有的提交时被拒（[`RemoteError::Rejected`]），
//!   下次拉的时候合并了再推。
//! - 凭据：不往仓库的 git 配置里写。https 远端的令牌放在环境变量 `TCMS_GIT_TOKEN`，经一个凭据助手交给 git，
//!   不出现在命令行参数和远端地址里；ssh 远端用部署者自己配的密钥。
//! - 每条命令有时限，网络慢、远端不回时不会一直占着提交队列；也不会停下来问密码。
//! - 这些命令不跑仓库里的钩子、不签名，和 gix 做的提交一样。

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use crate::{Author, ObjectId};

/// 网络命令（fetch、push、ls-remote）最多等多久。
const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);
/// 本地命令（merge、rev-parse 等）最多等多久。
const LOCAL_TIMEOUT: Duration = Duration::from_secs(60);
/// 错误信息里最多带 git 输出的多少个字符。
const MAX_MESSAGE: usize = 600;

/// 把环境变量 `TCMS_GIT_TOKEN` 交给 git 的凭据助手。git 用 sh 运行它，变量在那时才展开，
/// 所以令牌不出现在 tcms 传给 git 的参数里。用户名默认 `x-access-token`（GitHub 的写法），
/// Gitea、GitLab 用 `TCMS_GIT_USERNAME` 改成自己的用户名。
const TOKEN_HELPER: &str = r#"!f() { test "$1" = get || exit 0; echo "username=${TCMS_GIT_USERNAME:-x-access-token}"; echo "password=${TCMS_GIT_TOKEN}"; }; f"#;

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("远端配置不对：{0}")]
    BadConfig(String),
    #[error("没找到 git 命令行（配了远端时用它拉取、推送）")]
    GitMissing,
    #[error("HEAD 不在分支上，用 TCMS_REMOTE_BRANCH 写明推到远端的哪个分支")]
    Detached,
    #[error("工作区有没提交的改动，先提交或撤销再和远端同步")]
    Dirty,
    #[error("{}", needs_merge_message(.files, .branch, .side.as_deref()))]
    NeedsMerge {
        /// 有冲突的文件。
        files: Vec<String>,
        /// 远端的分支。
        branch: String,
        /// 服务器这边的提交推到了远端的这个旁支；推不上去时为空。
        side: Option<String>,
    },
    #[error("远端有本地没有的提交，推送被拒（不强推）；下次同步时先合并再推")]
    Rejected,
    #[error("git {what} 超过 {secs} 秒没做完，已停下")]
    Timeout { what: &'static str, secs: u64 },
    #[error("git {what} 失败：{message}")]
    Command { what: &'static str, message: String },
    #[error("运行 git {what}：{source}")]
    Io {
        what: &'static str,
        #[source]
        source: std::io::Error,
    },
}

/// 拉的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pulled {
    /// 远端还没有这个分支（第一次推之前）。
    NoBranch,
    UpToDate,
    /// 本地有远端没有的提交，远端没有新的。
    Ahead,
    /// 本地落后，快进到了远端。`from` 为空表示本地原来还没有提交。
    FastForward {
        from: Option<ObjectId>,
        to: ObjectId,
    },
    /// 两边都有新提交，合并成了 `commit`。
    Merged {
        commit: ObjectId,
        remote: ObjectId,
    },
}

impl Pulled {
    /// HEAD 变了没有。
    pub fn moved_head(&self) -> bool {
        matches!(self, Self::FastForward { .. } | Self::Merged { .. })
    }
}

impl std::fmt::Display for Pulled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoBranch => f.write_str("远端还没有这个分支"),
            Self::UpToDate => f.write_str("和远端一样"),
            Self::Ahead => f.write_str("远端没有新提交"),
            Self::FastForward { to, .. } => write!(f, "快进到远端的 {}", short(to)),
            Self::Merged { commit, remote } => {
                write!(
                    f,
                    "合并了远端的 {}，合并提交 {}",
                    short(remote),
                    short(commit)
                )
            }
        }
    }
}

/// 推的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pushed {
    /// 本地还没有提交，没什么可推。
    Nothing,
    /// 远端已经是这个提交了。
    UpToDate,
    Pushed {
        to: ObjectId,
    },
}

impl std::fmt::Display for Pushed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nothing => f.write_str("本地还没有提交"),
            Self::UpToDate => f.write_str("远端已经是最新的"),
            Self::Pushed { to } => write!(f, "推到了 {}", short(to)),
        }
    }
}

fn short(id: &ObjectId) -> String {
    id.to_string()[..12].to_owned()
}

/// 一个远端：往哪里推、从哪里拉。
#[derive(Debug, Clone)]
pub struct Remote {
    target: String,
    branch: Option<String>,
    token: bool,
}

impl Remote {
    /// `target` 是上下文仓库里配好的远端名（比如 `origin`），或者远端地址（`https://…`、`git@…:…`、本机路径）。
    /// `branch` 是远端的分支，不写时和本地当前分支同名。`token` 为真时，经凭据助手把环境变量
    /// `TCMS_GIT_TOKEN` 交给 git（https 远端用）。
    pub fn new(target: &str, branch: Option<&str>, token: bool) -> Result<Self, RemoteError> {
        let target = target.trim();
        if target.is_empty() {
            return Err(RemoteError::BadConfig("远端是空的".into()));
        }
        if target.starts_with('-') || target.chars().any(char::is_control) {
            return Err(RemoteError::BadConfig(format!(
                "远端不能以 - 开头、不能有控制字符：{}",
                redact(target)
            )));
        }
        let branch = match branch.map(str::trim).filter(|b| !b.is_empty()) {
            Some(b) if valid_branch(b) => Some(b.to_owned()),
            Some(b) => {
                return Err(RemoteError::BadConfig(format!("分支名不对：{b}")));
            }
            None => None,
        };
        Ok(Self {
            target: target.to_owned(),
            branch,
            token,
        })
    }

    /// 给人看的远端：地址里的用户名、密码换成 `***`。
    pub fn display(&self) -> String {
        let b = self.branch.as_deref().unwrap_or("和本地同名的分支");
        format!("{}（{b}）", redact(&self.target))
    }

    /// 从远端拉：快进或者合并。工作区要干净；合并不了时什么都不改，返回 [`RemoteError::NeedsMerge`]。
    /// 合并提交的作者和提交者是 `author`。
    pub async fn pull(&self, dir: &Path, author: &Author) -> Result<Pulled, RemoteError> {
        let git = Git { remote: self, dir };
        let branch = git.branch().await?;
        let refspec = format!("+refs/heads/{branch}");
        let fetched = git
            .run(
                "fetch",
                &[
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    "--",
                    &self.target,
                    &refspec,
                ],
                NETWORK_TIMEOUT,
                &[],
            )
            .await?;
        if !fetched.status.success() {
            // 分辨“远端没有这个分支”和别的错：ls-remote 没找到时退出码是 2，不靠 git 输出的语言。
            let probe = git
                .run(
                    "ls-remote",
                    &[
                        "ls-remote",
                        "--exit-code",
                        "--heads",
                        "--",
                        &self.target,
                        &format!("refs/heads/{branch}"),
                    ],
                    NETWORK_TIMEOUT,
                    &[],
                )
                .await?;
            if probe.status.code() == Some(2) {
                return Ok(Pulled::NoBranch);
            }
            return Err(failed("fetch", &fetched));
        }
        let remote = git
            .rev("FETCH_HEAD^{commit}")
            .await?
            .ok_or_else(|| RemoteError::Command {
                what: "fetch",
                message: "取回来以后没有 FETCH_HEAD".into(),
            })?;
        let local = git.rev("HEAD^{commit}").await?;
        if local == Some(remote) {
            return Ok(Pulled::UpToDate);
        }
        if let Some(local) = local
            && git.is_ancestor(remote, local).await?
        {
            return Ok(Pulled::Ahead);
        }
        if git.dirty().await? {
            return Err(RemoteError::Dirty);
        }
        let remote_hex = remote.to_string();
        let fast_forward = match local {
            None => true,
            Some(local) => git.is_ancestor(local, remote).await?,
        };
        if fast_forward {
            let out = git
                .run(
                    "merge",
                    &["merge", "--quiet", "--ff-only", &remote_hex],
                    LOCAL_TIMEOUT,
                    &[],
                )
                .await?;
            if !out.status.success() {
                return Err(failed("merge", &out));
            }
            return Ok(Pulled::FastForward {
                from: local,
                to: remote,
            });
        }

        let message = format!("合并远端的改动（{branch}）");
        let ident = [
            ("GIT_AUTHOR_NAME", author.name.as_str()),
            ("GIT_AUTHOR_EMAIL", author.email.as_str()),
            ("GIT_COMMITTER_NAME", author.name.as_str()),
            ("GIT_COMMITTER_EMAIL", author.email.as_str()),
        ];
        let out = git
            .run(
                "merge",
                &[
                    "merge",
                    "--quiet",
                    "--no-ff",
                    "--no-edit",
                    "-m",
                    &message,
                    &remote_hex,
                ],
                LOCAL_TIMEOUT,
                &ident,
            )
            .await?;
        if !out.status.success() {
            let files = git.conflicted().await.unwrap_or_default();
            // 合并到一半的状态要撤掉，不然工作区不干净，后面的提交都做不了。
            let _ = git
                .run("merge", &["merge", "--abort"], LOCAL_TIMEOUT, &[])
                .await;
            if files.is_empty() {
                return Err(failed("merge", &out));
            }
            // 容器里没有编辑器，人也不一定能上服务器：把这边的提交放到远端的旁支，让人在自己的克隆里合并。
            // 旁支只有 tcms 写，每次覆盖成服务器现在的 HEAD；推不上去就只能在服务器上合并了。
            let side = format!("tcms/{branch}-unmerged");
            let local = local.expect("两边都有新提交时本地一定有提交");
            let pushed = git
                .run(
                    "push",
                    &[
                        "push",
                        "--quiet",
                        "--",
                        &self.target,
                        &format!("+{local}:refs/heads/{side}"),
                    ],
                    NETWORK_TIMEOUT,
                    &[],
                )
                .await
                .is_ok_and(|o| o.status.success());
            return Err(RemoteError::NeedsMerge {
                files,
                branch,
                side: pushed.then_some(side),
            });
        }
        let commit = git
            .rev("HEAD^{commit}")
            .await?
            .ok_or(RemoteError::Detached)?;
        Ok(Pulled::Merged { commit, remote })
    }

    /// 把本地 HEAD 推到远端，不强推。远端有本地没有的提交时返回 [`RemoteError::Rejected`]。
    pub async fn push(&self, dir: &Path) -> Result<Pushed, RemoteError> {
        let git = Git { remote: self, dir };
        let Some(local) = git.rev("HEAD^{commit}").await? else {
            return Ok(Pushed::Nothing);
        };
        let branch = git.branch().await?;
        // 推具体的提交，不推 HEAD 这个名字：中途有别的提交也只推看到的这一个。
        let refspec = format!("{local}:refs/heads/{branch}");
        let out = git
            .run(
                "push",
                &["push", "--porcelain", "--", &self.target, &refspec],
                NETWORK_TIMEOUT,
                &[],
            )
            .await?;
        // --porcelain 每个引用一行：标记、Tab、引用、Tab、说明。`=` 没变，` ` `*` `+` 推上去了，`!` 被拒。
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            let mut cols = line.split('\t');
            let (Some(flag), Some(refs), summary) = (cols.next(), cols.next(), cols.next()) else {
                continue;
            };
            if !refs.ends_with(&format!(":refs/heads/{branch}")) {
                continue;
            }
            let summary = summary.unwrap_or_default();
            return match flag {
                "=" => Ok(Pushed::UpToDate),
                " " | "*" | "+" => Ok(Pushed::Pushed { to: local }),
                "!" if summary.starts_with("[rejected]") => Err(RemoteError::Rejected),
                _ => Err(RemoteError::Command {
                    what: "push",
                    message: clip(&redact(summary)),
                }),
            };
        }
        if out.status.success() {
            Ok(Pushed::Pushed { to: local })
        } else {
            Err(failed("push", &out))
        }
    }

    /// 每条 git 命令前面都加的参数。
    fn base_args(&self, dir: &Path) -> Vec<String> {
        let mut args = vec![
            "-C".to_owned(),
            dir.display().to_string(),
            // 仓库的属主和运行 tcms 的用户不同时（比如 root 运行命令行）git 会拒绝；这是 tcms 自己的仓库。
            "-c".to_owned(),
            format!("safe.directory={}", dir.display()),
            "-c".to_owned(),
            "core.hooksPath=/dev/null".to_owned(),
            "-c".to_owned(),
            "commit.gpgSign=false".to_owned(),
            "-c".to_owned(),
            "protocol.ext.allow=never".to_owned(),
            "-c".to_owned(),
            "core.quotePath=false".to_owned(),
        ];
        if self.token {
            // 先清掉别处配的凭据助手，只用环境变量里的令牌。
            for helper in ["", TOKEN_HELPER] {
                args.push("-c".to_owned());
                args.push(format!("credential.helper={helper}"));
            }
        }
        args
    }
}

struct Git<'a> {
    remote: &'a Remote,
    dir: &'a Path,
}

impl Git<'_> {
    async fn run(
        &self,
        what: &'static str,
        args: &[&str],
        timeout: Duration,
        env: &[(&str, &str)],
    ) -> Result<Output, RemoteError> {
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(self.remote.base_args(self.dir))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            // 不弹密码提示、ssh 不问要不要信任主机，等不到就失败。
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS")
            // 外面设的这些会让 git 去动别的仓库。
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
        if std::env::var_os("GIT_SSH_COMMAND").is_none() && std::env::var_os("GIT_SSH").is_none() {
            cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
        }
        for (k, v) in env {
            cmd.env(OsStr::new(k), OsStr::new(v));
        }
        let child = cmd.spawn().map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => RemoteError::GitMissing,
            _ => RemoteError::Io { what, source },
        })?;
        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(out) => out.map_err(|source| RemoteError::Io { what, source }),
            Err(_) => Err(RemoteError::Timeout {
                what,
                secs: timeout.as_secs(),
            }),
        }
    }

    /// 本地分支名，或者配置里写明的远端分支。
    async fn branch(&self) -> Result<String, RemoteError> {
        if let Some(b) = &self.remote.branch {
            return Ok(b.clone());
        }
        let out = self
            .run(
                "symbolic-ref",
                &["symbolic-ref", "--quiet", "--short", "HEAD"],
                LOCAL_TIMEOUT,
                &[],
            )
            .await?;
        let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !out.status.success() || !valid_branch(&name) {
            return Err(RemoteError::Detached);
        }
        Ok(name)
    }

    /// 解析一个引用；不存在时（比如还没有提交的 HEAD）返回 `None`。
    async fn rev(&self, spec: &str) -> Result<Option<ObjectId>, RemoteError> {
        let out = self
            .run(
                "rev-parse",
                &["rev-parse", "--quiet", "--verify", spec],
                LOCAL_TIMEOUT,
                &[],
            )
            .await?;
        if !out.status.success() {
            return Ok(None);
        }
        let hex = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        ObjectId::from_hex(hex.as_bytes())
            .map(Some)
            .map_err(|e| RemoteError::Command {
                what: "rev-parse",
                message: e.to_string(),
            })
    }

    /// `a` 是不是 `b` 的祖先（或者就是 `b`）。
    async fn is_ancestor(&self, a: ObjectId, b: ObjectId) -> Result<bool, RemoteError> {
        let out = self
            .run(
                "merge-base",
                &[
                    "merge-base",
                    "--is-ancestor",
                    &a.to_string(),
                    &b.to_string(),
                ],
                LOCAL_TIMEOUT,
                &[],
            )
            .await?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(failed("merge-base", &out)),
        }
    }

    /// 被跟踪的文件有没有没提交的改动（新放进来、没跟踪的文件不算，合并要覆盖它们时 git 自己会停）。
    async fn dirty(&self) -> Result<bool, RemoteError> {
        let out = self
            .run(
                "status",
                &["status", "--porcelain", "-z", "--untracked-files=no"],
                LOCAL_TIMEOUT,
                &[],
            )
            .await?;
        if !out.status.success() {
            return Err(failed("status", &out));
        }
        Ok(!out.stdout.is_empty())
    }

    /// 合并到一半时有冲突的文件。
    async fn conflicted(&self) -> Result<Vec<String>, RemoteError> {
        let out = self
            .run(
                "diff",
                &["diff", "--name-only", "-z", "--diff-filter=U"],
                LOCAL_TIMEOUT,
                &[],
            )
            .await?;
        Ok(out
            .stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect())
    }
}

fn needs_merge_message(files: &[String], branch: &str, side: Option<&str>) -> String {
    let head = format!(
        "本地和远端都有新提交，这些文件合并不了：{}。",
        files.join("、")
    );
    match side {
        Some(side) => format!(
            "{head}服务器这边的提交已经推到远端的 {side} 分支：在自己的克隆里 git fetch 远端 {side}、git merge FETCH_HEAD、解决冲突，推到 {branch}，下次同步时服务器快进过去"
        ),
        None => format!(
            "{head}要人在服务器的上下文仓库里合并：git merge FETCH_HEAD（tcms 刚取回来的远端提交），解决冲突，提交"
        ),
    }
}

fn failed(what: &'static str, out: &Output) -> RemoteError {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let text = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("hint:"))
        .collect::<Vec<_>>()
        .join("；");
    let message = if text.is_empty() {
        format!("退出码 {}", out.status.code().unwrap_or(-1))
    } else {
        clip(&redact(&text))
    };
    RemoteError::Command { what, message }
}

fn clip(s: &str) -> String {
    if s.chars().count() <= MAX_MESSAGE {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(MAX_MESSAGE).collect();
        out.push('…');
        out
    }
}

/// 把 `scheme://用户名:密码@主机` 里的用户名、密码换成 `***`，免得令牌写在地址里时进了日志。
pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| c == '/' || c.is_whitespace())
            .unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(at) => {
                out.push_str("***");
                out.push_str(&tail[at..end]);
            }
            None => out.push_str(&tail[..end]),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// 分支名：按 `git check-ref-format --branch` 的规则挡掉明显不对的。
fn valid_branch(b: &str) -> bool {
    !b.is_empty()
        && !b.starts_with('-')
        && !b.starts_with('/')
        && !b.ends_with('/')
        && !b.ends_with('.')
        && !b.ends_with(".lock")
        && b != "@"
        && !b.contains("..")
        && !b.contains("//")
        && !b.contains("@{")
        && !b
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || "~^:?*[\\".contains(c))
        && !b.split('/').any(|seg| seg.starts_with('.'))
}

#[cfg(test)]
mod tests;
