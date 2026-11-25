//! `tcms bench affine-doc`：AFFiNE 内核（`affine-doc`、`affine-store`）的基准，`docs/affine-kernel-rust.md` 的 K5。
//! 不要数据库，数据都是现造的：
//!
//! - 读一篇 1 千个块的文档：整篇的 Yjs 更新放进一个新的 Doc，读成块、写成 Markdown。
//! - 合并 1 万条更新：一个人在一段里一个字一个字地打出来的 1 万条小更新，用 yrs 合成一份；
//!   用的是 `affine-store` 的 `merge_updates`（放进 Doc 里应用再编码），对照着跑一次 yrs 的 `merge_updates_v1`；
//!   再写进本地库（`affine-store`，临时目录里的 SQLite 文件）、读一次（合成快照、写回、删掉合进去的）。
//! - 导出整个工作区：100 篇、每篇 100 个块，导出成 `.bs.zip`，再整篇导出成 Markdown。

use std::fmt::Write as _;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use affine_doc::md_export::{MdContext, snapshot_to_markdown};
use affine_doc::snapshot::to_snapshot;
use affine_doc::workspace::DocMeta;
use affine_doc::{RandomIds, bundle};
use anyhow::{Context, Result};
use yrs::types::text::TextRef;
use yrs::updates::decoder::Decode;
use yrs::{Any, Doc, Map, MapRef, Out, ReadTxn, StateVector, Text, Transact, Update};

use super::{ms, percentiles};

/// 读一篇 1 千个块的文档，p95 的门槛。
const READ_LIMIT: Duration = Duration::from_millis(25);
/// 合并 1 万条更新的门槛。
const MERGE_LIMIT: Duration = Duration::from_millis(50);
/// 1 万条更新写进本地库，一共的门槛。
const STORE_PUSH_LIMIT: Duration = Duration::from_secs(3);
/// 写进去以后第一次读（合成快照）的门槛。
const STORE_GET_LIMIT: Duration = Duration::from_millis(100);
/// 导出 100 篇、每篇 100 个块的工作区（`.bs.zip`）的门槛。
const EXPORT_LIMIT: Duration = Duration::from_millis(500);

const READ_RUNS: usize = 50;
const MERGE_RUNS: usize = 10;
const EXPORT_RUNS: usize = 5;

/// `n` 个块的 Markdown：小标题、正文（带粗体、代码、链接）、列表、编号、引用、代码块轮着来。
fn markdown(n: usize, seed: usize) -> String {
    let mut s = String::new();
    for i in 0..n {
        let k = i + seed;
        let _ = match i % 8 {
            0 => write!(s, "## 第 {k} 节"),
            1 | 2 => write!(
                s,
                "正文第 {k} 段：团队在周报里用表格，日期写全，**数字注明来源**和口径，`api_{k}` 见[说明](https://example.com/{k})。"
            ),
            3 => write!(s, "- 列表项 {k}\n  - 下一级 {k}"),
            4 => write!(s, "1. 编号 {k}"),
            5 => write!(s, "> 引用第 {k} 句"),
            6 => write!(s, "```rust\nlet x{k} = {k};\n```"),
            _ => write!(s, "- [ ] 待办 {k}"),
        };
        s.push_str("\n\n");
    }
    s
}

fn new_doc(title: &str, blocks: usize, seed: usize) -> Result<Doc> {
    let doc = Doc::new();
    affine_doc::create(
        &doc,
        title,
        &affine_doc::to_blocks(&markdown(blocks, seed)),
        &mut RandomIds,
    )?;
    Ok(doc)
}

fn full_update(doc: &Doc) -> Vec<u8> {
    doc.transact()
        .encode_state_as_update_v1(&StateVector::default())
}

/// 文档里第一个正文段落的文字。
fn first_paragraph(doc: &Doc) -> Option<TextRef> {
    let blocks: MapRef = doc.get_or_insert_map("blocks");
    let txn = doc.transact();
    for (_, v) in blocks.iter(&txn) {
        let Out::YMap(b) = v else { continue };
        let is_para = matches!(
            b.get(&txn, "sys:flavour"),
            Some(Out::Any(Any::String(f))) if &*f == "affine:paragraph"
        );
        if is_para && let Some(Out::YText(t)) = b.get(&txn, "prop:text") {
            return Some(t);
        }
    }
    None
}

/// 一个人在一段里打字：`n` 条小更新，每条加一到三个字。
fn typing_updates(n: usize) -> Result<(Vec<u8>, Vec<Vec<u8>>)> {
    let doc = Doc::new();
    affine_doc::create(&doc, "打字", &affine_doc::to_blocks("开头"), &mut RandomIds)?;
    let base = full_update(&doc);
    let text = first_paragraph(&doc).context("找不到正文段落")?;
    let words = ["数", "据", "来源", "口径", "，", "同比", "环比", "。"];
    let mut updates = Vec::with_capacity(n);
    for i in 0..n {
        let sv = doc.transact().state_vector();
        {
            let mut txn = doc.transact_mut();
            let len = text.len(&txn);
            text.insert(&mut txn, len, words[i % words.len()]);
        }
        updates.push(doc.transact().encode_state_as_update_v1(&sv));
    }
    Ok((base, updates))
}

pub fn run(check: bool) -> Result<ExitCode> {
    // 读一篇 1 千个块的文档
    let big = new_doc("一千个块", 1000, 0)?;
    let big_bin = full_update(&big);
    let blocks = affine_doc::read(&big)?.blocks.len();
    anyhow::ensure!(blocks >= 1000, "只造出了 {blocks} 个块");
    let mut reads = Vec::with_capacity(READ_RUNS);
    let mut md_len = 0;
    for _ in 0..READ_RUNS {
        let t = Instant::now();
        let doc = Doc::new();
        doc.transact_mut()
            .apply_update(Update::decode_v1(&big_bin)?)?;
        let page = affine_doc::read(&doc)?;
        md_len = affine_doc::to_markdown(&page.blocks).len();
        reads.push(t.elapsed());
    }
    let read = percentiles(reads);

    // 合并 1 万条更新
    let (base, updates) = typing_updates(10_000)?;
    let mut all: Vec<&[u8]> = vec![&base];
    all.extend(updates.iter().map(Vec::as_slice));
    let mut merges = Vec::with_capacity(MERGE_RUNS);
    let mut merged_len = 0;
    for _ in 0..MERGE_RUNS {
        let t = Instant::now();
        let merged = affine_store::merge_updates(&all)?;
        merges.push(t.elapsed());
        merged_len = merged.len();
    }
    // 对照：yrs 自己的 merge_updates_v1（affine-store 原来用它），只跑一次、不设门槛。
    let t = Instant::now();
    let yrs_merged = yrs::merge_updates_v1(all.clone())?;
    let yrs_merge = t.elapsed();
    anyhow::ensure!(!yrs_merged.is_empty() && merged_len > 0, "合出来是空的");
    let merge = percentiles(merges);
    let updates_len: usize = updates.iter().map(Vec::len).sum();

    // 写进本地库、读一次
    let dir = tempfile::tempdir()?;
    let mut store = affine_store::Store::open(dir.path().join("ws.db"))?;
    let t = Instant::now();
    for u in std::iter::once(&base).chain(&updates) {
        store.push_update("d1", u)?;
    }
    let store_push = t.elapsed();
    let t = Instant::now();
    let got = store.get_doc("d1")?.context("库里没有这篇")?;
    let store_get = t.elapsed();
    let t = Instant::now();
    store.get_doc("d1")?;
    let store_get_again = t.elapsed();
    anyhow::ensure!(
        store.pending_updates("d1")? == 0,
        "读过以后还有没合进去的更新"
    );
    let check_doc = Doc::new();
    check_doc
        .transact_mut()
        .apply_update(Update::decode_v1(&got.bin)?)?;
    anyhow::ensure!(
        affine_doc::to_markdown(&affine_doc::read(&check_doc)?.blocks)
            .chars()
            .count()
            > 10_000,
        "合出来的文档少了字"
    );

    // 导出整个工作区
    let docs: Vec<(Doc, DocMeta)> = (0..100)
        .map(|i| {
            let id = format!("doc{i:03}");
            let title = format!("第 {i} 篇");
            Ok((
                new_doc(&title, 100, i * 100)?,
                DocMeta::new(id, title, 1.7e12),
            ))
        })
        .collect::<Result<_>>()?;
    let refs: Vec<(&Doc, &DocMeta)> = docs.iter().map(|(d, m)| (d, m)).collect();
    let mut exports = Vec::with_capacity(EXPORT_RUNS);
    let mut zip_len = 0;
    for _ in 0..EXPORT_RUNS {
        let t = Instant::now();
        let out = bundle::export_docs(&refs, |_| None)?;
        exports.push(t.elapsed());
        zip_len = out.zip.len();
    }
    let export = percentiles(exports);
    let cx = MdContext::default();
    let mut md_exports = Vec::with_capacity(EXPORT_RUNS);
    for _ in 0..EXPORT_RUNS {
        let t = Instant::now();
        for (d, m) in &docs {
            let snap = to_snapshot(d, m)?;
            snapshot_to_markdown(&snap, &cx);
        }
        md_exports.push(t.elapsed());
    }
    let md_export = percentiles(md_exports);

    println!("AFFiNE 内核基准");
    println!(
        "  读一篇 {blocks} 个块的文档（{} KB，Markdown {} KB）  中位数 {:>6.2} 毫秒  p95 {:>6.2} 毫秒   门槛 p95 {:.0} 毫秒",
        big_bin.len() / 1024,
        md_len / 1024,
        ms(read.p50),
        ms(read.p95),
        ms(READ_LIMIT)
    );
    println!(
        "  合并 1 万条更新（{} KB → {} KB）                中位数 {:>6.2} 毫秒  p95 {:>6.2} 毫秒   门槛 {:.0} 毫秒",
        updates_len / 1024,
        merged_len / 1024,
        ms(merge.p50),
        ms(merge.p95),
        ms(MERGE_LIMIT)
    );
    println!(
        "  对照：yrs 的 merge_updates_v1 合同样 1 万条       {:>7.1} 毫秒（只跑一次）",
        ms(yrs_merge)
    );
    println!(
        "  1 万条更新写进本地库                           一共 {:>7.1} 毫秒（每条 {:.1} 微秒）  门槛 {:.0} 毫秒",
        ms(store_push),
        store_push.as_secs_f64() * 1e6 / (updates.len() + 1) as f64,
        ms(STORE_PUSH_LIMIT)
    );
    println!(
        "  写进去以后第一次读（合成快照）                 {:>7.1} 毫秒   门槛 {:.0} 毫秒；再读 {:.2} 毫秒",
        ms(store_get),
        ms(STORE_GET_LIMIT),
        ms(store_get_again)
    );
    println!(
        "  导出 100 篇、每篇 100 个块（.bs.zip {} KB）     中位数 {:>6.1} 毫秒  p95 {:>6.1} 毫秒   门槛 {:.0} 毫秒",
        zip_len / 1024,
        ms(export.p50),
        ms(export.p95),
        ms(EXPORT_LIMIT)
    );
    println!(
        "  同样 100 篇整篇导出成 Markdown                 中位数 {:>6.1} 毫秒  p95 {:>6.1} 毫秒",
        ms(md_export.p50),
        ms(md_export.p95)
    );

    let mut failed = false;
    for (name, got, limit) in [
        ("读一篇 1 千个块的文档", read.p95, READ_LIMIT),
        ("合并 1 万条更新", merge.p95, MERGE_LIMIT),
        ("1 万条更新写进本地库", store_push, STORE_PUSH_LIMIT),
        ("写进去以后第一次读", store_get, STORE_GET_LIMIT),
        ("导出整个工作区", export.p95, EXPORT_LIMIT),
    ] {
        if got > limit {
            eprintln!("{name}超过门槛");
            failed = true;
        }
    }
    Ok(if check && failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
