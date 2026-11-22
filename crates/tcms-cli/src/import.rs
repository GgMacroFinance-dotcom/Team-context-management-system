//! `tcms import`：把一个 Markdown 目录整理成上下文仓库的格式，做成一个提交（`docs/spec.md` 6.3）。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tcms_core::{Scope, assign_ids, clean_export_name, parse_file};
use tcms_git::{Author, ContextRepo, FileEdit};

pub fn run(repo: &ContextRepo, src: &Path, into: &str, author: &Author) -> Result<()> {
    let into = into.trim_matches('/');
    let first = into.split('/').next().unwrap_or_default();
    if Scope::from_dir(first).is_none() {
        bail!("--into 的第一层必须是范围目录（org、team-x、role-x、project-x），现在是 {first}");
    }

    let mut files = Vec::new();
    walk(src, src, &mut files)?;
    files.sort();

    let existing = match repo.head()? {
        Some(head) => repo.files(head)?,
        None => Default::default(),
    };

    let mut edits = Vec::new();
    let mut skipped = Vec::new();
    let mut conflicts = Vec::new();
    let mut items = 0;
    let mut warnings = Vec::new();
    for rel in files {
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if !rel_str.ends_with(".md") {
            skipped.push(rel_str);
            continue;
        }
        let cleaned: Vec<String> = rel_str.split('/').map(clean_export_name).collect();
        let target = format!("{into}/{}", cleaned.join("/"));
        if existing.contains_key(&target) || edits.iter().any(|e: &FileEdit| e.path == target) {
            conflicts.push(target);
            continue;
        }
        let bytes = std::fs::read(src.join(&rel))
            .with_context(|| format!("读 {}", src.join(&rel).display()))?;
        let Ok(text) = String::from_utf8(bytes) else {
            skipped.push(format!("{rel_str}（不是 UTF-8）"));
            continue;
        };
        let text = match assign_ids(&target, &text) {
            Some((new, _)) => new,
            None => text,
        };
        let parsed = parse_file(&target, &text);
        items += parsed.items.len();
        warnings.extend(parsed.warnings);
        edits.push(FileEdit {
            path: target,
            content: Some(text.into_bytes()),
        });
    }

    if !conflicts.is_empty() {
        bail!(
            "这些文件仓库里已经有了，不覆盖：\n  {}",
            conflicts.join("\n  ")
        );
    }
    if edits.is_empty() {
        bail!("{} 里没有 .md 文件", src.display());
    }
    let message = format!(
        "导入 {} 个文件到 {into}（{items} 条）\n\n来源：{}\n",
        edits.len(),
        src.display()
    );
    let commit = repo.commit(&edits, &message, author)?;
    println!(
        "导入了 {} 个文件、{items} 条到 {into}，提交 {commit}。",
        edits.len()
    );
    if !skipped.is_empty() {
        println!("跳过 {} 个不是 Markdown 的文件：", skipped.len());
        for s in &skipped {
            println!("  {s}");
        }
    }
    crate::print_warnings(&warnings);
    println!("接着运行 tcms reindex 更新索引。");
    Ok(())
}

/// 递归列出 `dir` 下的文件，返回相对 `root` 的路径。跳过隐藏文件和目录。
fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("读目录 {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let ty = entry.file_type()?;
        if ty.is_dir() {
            walk(root, &path, out)?;
        } else if ty.is_file() {
            out.push(path.strip_prefix(root).expect("在 root 下面").to_owned());
        }
    }
    Ok(())
}
