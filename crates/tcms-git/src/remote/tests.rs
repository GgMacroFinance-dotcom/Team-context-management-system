//! 远端的测试：本机的裸仓库当远端，另一个克隆当“人”。要有 git 命令行；没有时跳过（CI 里有）。

use std::path::Path;
use std::process::Command;

use super::*;
use crate::{ContextRepo, FileEdit};

fn have_git() -> bool {
    let ok = Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        assert!(std::env::var_os("CI").is_none(), "CI 里要有 git 命令行");
        eprintln!("没有 git 命令行，跳过");
    }
    ok
}

/// 测试里自己调 git：不签名、不跑钩子、提交者固定，和机器上的全局配置无关。
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(["-c", "user.name=人", "-c", "user.email=human@example.com"])
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "core.quotePath=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn edit(path: &str, content: &str) -> FileEdit {
    FileEdit {
        path: path.to_owned(),
        content: Some(content.as_bytes().to_vec()),
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    /// 服务这边的上下文仓库（用 gix 提交）。
    server: std::path::PathBuf,
    /// 远端（裸仓库）。
    bare: std::path::PathBuf,
    /// 人自己的克隆。
    human: std::path::PathBuf,
    remote: Remote,
}

/// 服务这边有一个提交，推到空的远端，人再克隆一份。
async fn setup() -> Option<Setup> {
    if !have_git() {
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let server = dir.path().join("server");
    let bare = dir.path().join("remote.git");
    let human = dir.path().join("human");
    git(dir.path(), &["init", "-q", "-b", "main", "server"]);
    git(dir.path(), &["init", "-q", "--bare", "remote.git"]);
    let repo = ContextRepo::open(&server).unwrap();
    repo.commit(
        &[
            edit("org/规矩.md", "## 一\n第一条\n"),
            edit("org/b.md", "b\n"),
        ],
        "第一个",
        &Author::default(),
    )
    .unwrap();
    let remote = Remote::new(bare.to_str().unwrap(), None, false).unwrap();
    assert_eq!(
        remote.pull(&server, &Author::default()).await.unwrap(),
        Pulled::NoBranch
    );
    let head = repo.head().unwrap().unwrap();
    assert_eq!(
        remote.push(&server).await.unwrap(),
        Pushed::Pushed { to: head }
    );
    git(
        dir.path(),
        &["clone", "-q", bare.to_str().unwrap(), "human"],
    );
    Some(Setup {
        _dir: dir,
        server,
        bare,
        human,
        remote,
    })
}

#[tokio::test]
async fn push_then_up_to_date() {
    let Some(s) = setup().await else { return };
    assert_eq!(s.remote.push(&s.server).await.unwrap(), Pushed::UpToDate);
    assert_eq!(
        s.remote.pull(&s.server, &Author::default()).await.unwrap(),
        Pulled::UpToDate
    );
    assert_eq!(
        git(&s.bare, &["rev-parse", "refs/heads/main"]),
        git(&s.server, &["rev-parse", "HEAD"])
    );
    // 服务这边又提交了一个，远端没有新的：拉的时候是 Ahead，推上去。
    let repo = ContextRepo::open(&s.server).unwrap();
    let c2 = repo
        .commit(&[edit("org/c.md", "c\n")], "第二个", &Author::default())
        .unwrap();
    assert_eq!(
        s.remote.pull(&s.server, &Author::default()).await.unwrap(),
        Pulled::Ahead
    );
    assert_eq!(
        s.remote.push(&s.server).await.unwrap(),
        Pushed::Pushed { to: c2 }
    );
}

#[tokio::test]
async fn fast_forward_then_merge() {
    let Some(s) = setup().await else { return };
    // 先打开，模拟批准的时候：仓库对象在拉之前就打开了，拉完接着用它提交。
    let repo = ContextRepo::open(&s.server).unwrap();
    let before = repo.head().unwrap();

    std::fs::write(s.human.join("org/人加的.md"), "## 人加的\n内容\n").unwrap();
    git(&s.human, &["add", "."]);
    git(&s.human, &["commit", "-q", "-m", "人加了一个文件"]);
    git(&s.human, &["push", "-q", "origin", "main"]);
    let human_head = git(&s.human, &["rev-parse", "HEAD"]);

    let pulled = s.remote.pull(&s.server, &Author::default()).await.unwrap();
    let to = ObjectId::from_hex(human_head.as_bytes()).unwrap();
    assert_eq!(pulled, Pulled::FastForward { from: before, to });
    assert!(pulled.moved_head());
    assert_eq!(repo.head().unwrap(), Some(to));
    assert_eq!(
        std::fs::read_to_string(s.server.join("org/人加的.md")).unwrap(),
        "## 人加的\n内容\n"
    );
    assert_eq!(git(&s.server, &["status", "--porcelain"]), "");

    // 两边各改一个不同的文件：合并成一个合并提交。
    let mine = repo
        .commit(
            &[edit("org/b.md", "b 改了\n")],
            "服务改 b",
            &Author::default(),
        )
        .unwrap();
    std::fs::write(s.human.join("org/人加的.md"), "## 人加的\n人又改了\n").unwrap();
    git(&s.human, &["commit", "-q", "-am", "人又改了"]);
    git(&s.human, &["push", "-q", "origin", "main"]);

    let author = Author {
        name: "审核的人".into(),
        email: "reviewer@example.com".into(),
    };
    let Pulled::Merged { commit, remote } = s.remote.pull(&s.server, &author).await.unwrap() else {
        panic!("应该合并");
    };
    assert_eq!(remote.to_string(), git(&s.human, &["rev-parse", "HEAD"]));
    assert_eq!(repo.head().unwrap(), Some(commit));
    assert_eq!(
        git(&s.server, &["log", "-1", "--format=%an <%ae>|%s|%P"]),
        format!("审核的人 <reviewer@example.com>|合并远端的改动（main）|{mine} {remote}")
    );
    assert_eq!(
        std::fs::read_to_string(s.server.join("org/b.md")).unwrap(),
        "b 改了\n"
    );
    assert_eq!(
        std::fs::read_to_string(s.server.join("org/人加的.md")).unwrap(),
        "## 人加的\n人又改了\n"
    );
    assert_eq!(git(&s.server, &["status", "--porcelain"]), "");

    // gix 照样能接着提交（工作区、暂存区是 git 命令行刚写的）。
    let after = repo
        .commit(&[edit("org/d.md", "d\n")], "合并以后", &Author::default())
        .unwrap();
    assert_eq!(
        s.remote.push(&s.server).await.unwrap(),
        Pushed::Pushed { to: after }
    );
    git(&s.human, &["pull", "-q", "--ff-only"]);
    assert_eq!(git(&s.human, &["rev-parse", "HEAD"]), after.to_string());
}

#[tokio::test]
async fn conflict_leaves_repo_as_it_was() {
    let Some(s) = setup().await else { return };
    let repo = ContextRepo::open(&s.server).unwrap();
    let mine = repo
        .commit(
            &[edit("org/规矩.md", "## 一\n服务这边改的\n")],
            "服务改",
            &Author::default(),
        )
        .unwrap();
    std::fs::write(s.human.join("org/规矩.md"), "## 一\n人改的\n").unwrap();
    git(&s.human, &["commit", "-q", "-am", "人改"]);
    git(&s.human, &["push", "-q", "origin", "main"]);

    let err = s
        .remote
        .pull(&s.server, &Author::default())
        .await
        .unwrap_err();
    match &err {
        RemoteError::NeedsMerge {
            files,
            branch,
            side,
        } => {
            assert_eq!(files, &["org/规矩.md"]);
            assert_eq!(branch, "main");
            assert_eq!(side.as_deref(), Some("tcms/main-unmerged"));
        }
        other => panic!("应该是合并不了：{other}"),
    }
    let msg = err.to_string();
    assert!(
        msg.contains("org/规矩.md") && msg.contains("tcms/main-unmerged"),
        "{msg}"
    );
    // 服务器这边的提交放在远端的旁支上；主分支没动。
    assert_eq!(
        git(&s.bare, &["rev-parse", "refs/heads/tcms/main-unmerged"]),
        mine.to_string()
    );
    // 什么都没改：HEAD 还是原来的，工作区干净，没有合并到一半的状态。
    assert_eq!(repo.head().unwrap(), Some(mine));
    assert_eq!(git(&s.server, &["status", "--porcelain"]), "");
    assert!(!s.server.join(".git/MERGE_HEAD").exists());
    assert_eq!(
        std::fs::read_to_string(s.server.join("org/规矩.md")).unwrap(),
        "## 一\n服务这边改的\n"
    );
    // 推也推不上去，不强推。
    assert!(matches!(
        s.remote.push(&s.server).await,
        Err(RemoteError::Rejected)
    ));
    assert_eq!(
        git(&s.bare, &["rev-parse", "refs/heads/main"]),
        git(&s.human, &["rev-parse", "HEAD"])
    );
    // 后面的提交照样能做（工作区干净）。
    let later = repo
        .commit(&[edit("org/e.md", "e\n")], "接着提交", &Author::default())
        .unwrap();
    // 再拉一次：旁支跟着覆盖成服务器现在的 HEAD。
    assert!(matches!(
        s.remote.pull(&s.server, &Author::default()).await,
        Err(RemoteError::NeedsMerge { .. })
    ));
    assert_eq!(
        git(&s.bare, &["rev-parse", "refs/heads/tcms/main-unmerged"]),
        later.to_string()
    );

    // 人在自己的克隆里合并旁支，推到 main；服务器下次拉的时候快进过去。
    git(&s.human, &["fetch", "-q", "origin", "tcms/main-unmerged"]);
    let merge = Command::new("git")
        .args([
            "-c",
            "commit.gpgSign=false",
            "-c",
            "user.name=人",
            "-c",
            "user.email=human@example.com",
        ])
        .args(["merge", "-q", "--no-edit", "FETCH_HEAD"])
        .current_dir(&s.human)
        .output()
        .unwrap();
    assert!(!merge.status.success(), "应该有冲突");
    std::fs::write(s.human.join("org/规矩.md"), "## 一\n人合并好的\n").unwrap();
    git(&s.human, &["add", "org/规矩.md"]);
    git(&s.human, &["commit", "-q", "--no-edit"]);
    git(&s.human, &["push", "-q", "origin", "main"]);
    let pulled = s.remote.pull(&s.server, &Author::default()).await.unwrap();
    assert!(matches!(pulled, Pulled::FastForward { .. }), "{pulled:?}");
    assert_eq!(
        std::fs::read_to_string(s.server.join("org/规矩.md")).unwrap(),
        "## 一\n人合并好的\n"
    );
    assert!(s.server.join("org/e.md").exists());
}

#[tokio::test]
async fn push_rejected_when_remote_moved() {
    let Some(s) = setup().await else { return };
    let repo = ContextRepo::open(&s.server).unwrap();
    repo.commit(&[edit("org/c.md", "c\n")], "服务", &Author::default())
        .unwrap();
    std::fs::write(s.human.join("org/h.md"), "h\n").unwrap();
    git(&s.human, &["add", "."]);
    git(&s.human, &["commit", "-q", "-m", "人"]);
    git(&s.human, &["push", "-q", "origin", "main"]);
    assert!(matches!(
        s.remote.push(&s.server).await,
        Err(RemoteError::Rejected)
    ));
    // 拉的时候合并，再推就上去了。
    assert!(matches!(
        s.remote.pull(&s.server, &Author::default()).await.unwrap(),
        Pulled::Merged { .. }
    ));
    assert!(matches!(
        s.remote.push(&s.server).await.unwrap(),
        Pushed::Pushed { .. }
    ));
}

#[tokio::test]
async fn empty_repo_takes_the_remote() {
    let Some(s) = setup().await else { return };
    let fresh = s._dir.path().join("fresh");
    git(s._dir.path(), &["init", "-q", "-b", "main", "fresh"]);
    let remote = Remote::new(s.bare.to_str().unwrap(), Some("main"), false).unwrap();
    assert_eq!(remote.push(&fresh).await.unwrap(), Pushed::Nothing);
    let pulled = remote.pull(&fresh, &Author::default()).await.unwrap();
    let Pulled::FastForward { from: None, to } = pulled else {
        panic!("空仓库应该快进：{pulled:?}");
    };
    assert_eq!(ContextRepo::open(&fresh).unwrap().head().unwrap(), Some(to));
    assert!(fresh.join("org/规矩.md").exists());
}

#[tokio::test]
async fn dirty_worktree_is_left_alone() {
    let Some(s) = setup().await else { return };
    std::fs::write(s.human.join("org/h.md"), "h\n").unwrap();
    git(&s.human, &["add", "."]);
    git(&s.human, &["commit", "-q", "-m", "人"]);
    git(&s.human, &["push", "-q", "origin", "main"]);
    std::fs::write(s.server.join("org/b.md"), "没提交的改动\n").unwrap();
    assert!(matches!(
        s.remote.pull(&s.server, &Author::default()).await,
        Err(RemoteError::Dirty)
    ));
    assert_eq!(
        std::fs::read_to_string(s.server.join("org/b.md")).unwrap(),
        "没提交的改动\n"
    );
}

#[tokio::test]
async fn unreachable_remote_is_an_error_not_a_hang() {
    let Some(s) = setup().await else { return };
    let missing = s._dir.path().join("没有这个仓库.git");
    let remote = Remote::new(missing.to_str().unwrap(), None, false).unwrap();
    let err = remote
        .pull(&s.server, &Author::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, RemoteError::Command { what: "fetch", .. }),
        "{err}"
    );
    assert!(matches!(
        remote.push(&s.server).await,
        Err(RemoteError::Command { what: "push", .. })
    ));
}

/// 令牌只经凭据助手从环境变量交给 git，不在参数里。
#[test]
fn token_reaches_git_only_through_the_helper() {
    if !have_git() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let remote = Remote::new("https://git.example.com/team/context.git", None, true).unwrap();
    let args = remote.base_args(dir.path());
    let secret = "tok-只在环境变量里-123";
    assert!(args.iter().all(|a| !a.contains(secret)));
    let run = |username: Option<&str>| {
        let mut cmd = Command::new("git");
        cmd.args(&args)
            .args(["credential", "fill"])
            .env("TCMS_GIT_TOKEN", secret)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match username {
            Some(u) => cmd.env("TCMS_GIT_USERNAME", u),
            None => cmd.env_remove("TCMS_GIT_USERNAME"),
        };
        let mut child = cmd.spawn().unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"protocol=https\nhost=git.example.com\n\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let out = run(None);
    assert!(out.contains("username=x-access-token\n"), "{out}");
    assert!(out.contains(&format!("password={secret}\n")), "{out}");
    let out = run(Some("gitea-user"));
    assert!(out.contains("username=gitea-user\n"), "{out}");

    // 不开令牌时不加凭据助手。
    let plain = Remote::new("https://git.example.com/x.git", None, false).unwrap();
    assert!(
        plain
            .base_args(dir.path())
            .iter()
            .all(|a| !a.starts_with("credential.helper"))
    );
}

#[test]
fn redacts_userinfo_in_urls() {
    assert_eq!(
        redact("https://user:tok123@github.com/a/b.git"),
        "https://***@github.com/a/b.git"
    );
    assert_eq!(
        redact("fatal: unable to access 'https://tok@git.example.com/x/': 403"),
        "fatal: unable to access 'https://***@git.example.com/x/': 403"
    );
    assert_eq!(redact("git@github.com:a/b.git"), "git@github.com:a/b.git");
    assert_eq!(redact("https://github.com/a@b"), "https://github.com/a@b");
    assert_eq!(redact("ssh://git@host/x"), "ssh://***@host/x");
}

#[test]
fn rejects_bad_config() {
    assert!(Remote::new("", None, false).is_err());
    assert!(Remote::new("--upload-pack=x", None, false).is_err());
    assert!(Remote::new("origin\nx", None, false).is_err());
    for b in [
        "-x", "a..b", "a b", "a:b", "x.lock", "a/.b", "/a", "a/", "@",
    ] {
        assert!(Remote::new("origin", Some(b), false).is_err(), "{b}");
    }
    let r = Remote::new(" origin ", Some("team/main"), false).unwrap();
    assert_eq!(r.display(), "origin（team/main）");
    let r = Remote::new("https://u:p@h/x.git", Some(""), false).unwrap();
    assert_eq!(r.display(), "https://***@h/x.git（和本地同名的分支）");
}
