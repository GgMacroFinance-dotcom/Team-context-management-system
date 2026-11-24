//! 按标题块把正文切成条目（和上下文仓库的规矩一样，`docs/architecture.md` 第 2 节）：
//! 最上一层的二级标题开始一条，到下一个一级或二级标题为止；不在任何二级标题下面的块不算条目。

use crate::markdown::to_markdown;
use crate::model::{Block, Content, plain_text};

/// 正文里的一段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Section {
    /// 不属于任何一条的块（第一个二级标题前面的、一级标题和它后面的）。
    Loose(Vec<Block>),
    Item(Item),
}

/// 一条。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// 二级标题块的 id。从 Markdown 切出来的没有。
    pub heading_id: Option<String>,
    /// 标题的文字（去掉格式，换行换成空格，去掉前后空白）。
    pub title: String,
    pub body: Vec<Block>,
    /// 标题或正文转成 Markdown 时丢了东西。
    pub lossy: bool,
}

impl Item {
    /// 正文的 Markdown。
    pub fn body_markdown(&self) -> String {
        to_markdown(&self.body)
    }
}

/// 切开。
pub fn split(blocks: &[Block]) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    for b in blocks {
        match b.heading_level() {
            Some(2) => {
                let (title, formatted) = match &b.content {
                    Content::Paragraph { text, .. } => (
                        plain_text(text).replace('\n', " ").trim().to_owned(),
                        text.iter().any(|s| s.marks != Default::default()),
                    ),
                    _ => (String::new(), false),
                };
                out.push(Section::Item(Item {
                    heading_id: b.id.clone(),
                    title,
                    body: Vec::new(),
                    lossy: formatted || b.is_lossy(),
                }));
            }
            Some(1) => push_loose(&mut out, b),
            _ => match out.last_mut() {
                Some(Section::Item(item)) => {
                    item.lossy |= b.is_lossy();
                    item.body.push(b.clone());
                }
                _ => push_loose(&mut out, b),
            },
        }
    }
    out
}

fn push_loose(out: &mut Vec<Section>, b: &Block) {
    match out.last_mut() {
        Some(Section::Loose(v)) => v.push(b.clone()),
        _ => out.push(Section::Loose(vec![b.clone()])),
    }
}
