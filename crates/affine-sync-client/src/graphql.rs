//! 用得到的几个 GraphQL 接口：工作区、最近改过的文档、评论和回复。
//!
//! 查询的写法照 AFFiNE 网页版用的那几个（`packages/common/graphql` 里的 `.gql`，MIT），只要用得到的字段。

use serde_json::{Value, json};

use crate::Error;
use crate::client::Client;

/// 这个账号能看到的一个工作区。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceInfo {
    pub id: String,
    /// 服务端有没有这个工作区的根文档（新建的工作区要先推一个上去）。
    pub initialized: bool,
}

/// 最近改过的一篇文档。时间是服务端给的 ISO 8601 字符串。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocInfo {
    pub id: String,
    pub title: Option<String>,
    pub updated_at: String,
    pub last_updater_id: Option<String>,
}

/// 一页结果和下一页从哪里开始。
#[derive(Clone, Debug)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub end_cursor: Option<String>,
    pub has_next_page: bool,
}

/// 文档上的一条评论（回复也是这个样子，`replies` 是空的）。
#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    pub id: String,
    /// `{snapshot, preview, mode, attachments}`，`snapshot` 是一小篇 BlockSuite 文档的快照。
    pub content: Value,
    pub resolved: bool,
    pub user_id: String,
    /// 发的人在 AFFiNE 里的名字。
    pub user_name: String,
    pub created_at: String,
    pub updated_at: String,
    pub replies: Vec<Comment>,
}

/// 评论的一次变化（用来增量地看评论）。
#[derive(Clone, Debug, PartialEq)]
pub struct CommentChange {
    /// `update`（新建或修改）或者 `delete`。
    pub action: String,
    pub id: String,
    /// 是回复时，它属于哪条评论。
    pub comment_id: Option<String>,
    pub item: Value,
}

/// 文档的模式：页面还是白板。评论要写。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocMode {
    Page,
    Edgeless,
}

const COMMENT_FIELDS: &str = "id content resolved createdAt updatedAt user { id name } replies { id content createdAt updatedAt user { id name } }";
const REPLY_FIELDS: &str = "id content createdAt updatedAt user { id name }";

impl Client {
    pub async fn workspaces(&self) -> Result<Vec<WorkspaceInfo>, Error> {
        let data = self
            .graphql("query { workspaces { id initialized } }", json!({}))
            .await?;
        array(&data, &["workspaces"])?
            .iter()
            .map(|w| {
                Ok(WorkspaceInfo {
                    id: string(w, "id")?,
                    initialized: w
                        .get("initialized")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            })
            .collect()
    }

    /// 新建一个工作区，返回它的 id。服务端这时还没有根文档，要自己推一个（见 `affine-doc` 的 `workspace`）。
    pub async fn create_workspace(&self) -> Result<String, Error> {
        let data = self
            .graphql("mutation { createWorkspace { id } }", json!({}))
            .await?;
        string(get(&data, &["createWorkspace"])?, "id")
    }

    /// 按改的时间从新到旧列文档，一页 `first` 篇。
    pub async fn recently_updated_docs(
        &self,
        workspace_id: &str,
        first: u32,
        after: Option<&str>,
    ) -> Result<Page<DocInfo>, Error> {
        let data = self
            .graphql(
                "query($workspaceId: String!, $pagination: PaginationInput!) {
                   workspace(id: $workspaceId) {
                     recentlyUpdatedDocs(pagination: $pagination) {
                       pageInfo { endCursor hasNextPage }
                       edges { node { id title updatedAt lastUpdaterId } }
                     }
                   }
                 }",
                json!({ "workspaceId": workspace_id, "pagination": { "first": first, "after": after } }),
            )
            .await?;
        let conn = get(&data, &["workspace", "recentlyUpdatedDocs"])?;
        page(conn, |n| {
            Ok(DocInfo {
                id: string(n, "id")?,
                title: opt_string(n, "title"),
                updated_at: string(n, "updatedAt")?,
                last_updater_id: opt_string(n, "lastUpdaterId"),
            })
        })
    }

    /// 一篇文档上的所有评论（自己翻页）。
    pub async fn comments(&self, workspace_id: &str, doc_id: &str) -> Result<Vec<Comment>, Error> {
        let query = format!(
            "query($workspaceId: String!, $docId: String!, $pagination: PaginationInput) {{
               workspace(id: $workspaceId) {{
                 comments(docId: $docId, pagination: $pagination) {{
                   pageInfo {{ endCursor hasNextPage }}
                   edges {{ node {{ {COMMENT_FIELDS} }} }}
                 }}
               }}
             }}"
        );
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let data = self
                .graphql(
                    &query,
                    json!({ "workspaceId": workspace_id, "docId": doc_id, "pagination": { "first": 100, "after": after } }),
                )
                .await?;
            let p = page(get(&data, &["workspace", "comments"])?, comment)?;
            out.extend(p.items);
            match (p.has_next_page, p.end_cursor) {
                (true, Some(c)) if after.as_deref() != Some(c.as_str()) => after = Some(c),
                _ => return Ok(out),
            }
        }
    }

    /// 发一条评论。`content` 用 [`comment_content`] 做。
    pub async fn create_comment(
        &self,
        workspace_id: &str,
        doc_id: &str,
        doc_title: &str,
        mode: DocMode,
        content: Value,
    ) -> Result<Comment, Error> {
        let query = format!(
            "mutation($input: CommentCreateInput!) {{ createComment(input: $input) {{ {COMMENT_FIELDS} }} }}"
        );
        let data = self
            .graphql(
                &query,
                json!({ "input": {
                    "workspaceId": workspace_id,
                    "docId": doc_id,
                    "docTitle": doc_title,
                    "docMode": match mode { DocMode::Page => "page", DocMode::Edgeless => "edgeless" },
                    "content": content,
                }}),
            )
            .await?;
        comment(get(&data, &["createComment"])?)
    }

    /// 回复一条评论，返回这条回复（`replies` 是空的、`resolved` 是 `false`）。`doc_title`、`mode` 和发评论时一样，
    /// 服务端拿来写通知。
    pub async fn create_reply(
        &self,
        comment_id: &str,
        doc_title: &str,
        mode: DocMode,
        content: Value,
    ) -> Result<Comment, Error> {
        let query = format!(
            "mutation($input: ReplyCreateInput!) {{ createReply(input: $input) {{ {REPLY_FIELDS} }} }}"
        );
        let data = self
            .graphql(
                &query,
                json!({ "input": {
                    "commentId": comment_id,
                    "docTitle": doc_title,
                    "docMode": match mode { DocMode::Page => "page", DocMode::Edgeless => "edgeless" },
                    "content": content,
                }}),
            )
            .await?;
        comment(get(&data, &["createReply"])?)
    }

    /// 把评论标成已解决（或者取消）。
    pub async fn resolve_comment(&self, comment_id: &str, resolved: bool) -> Result<(), Error> {
        self.graphql(
            "mutation($input: CommentResolveInput!) { resolveComment(input: $input) }",
            json!({ "input": { "id": comment_id, "resolved": resolved } }),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_comment(&self, comment_id: &str) -> Result<(), Error> {
        self.graphql(
            "mutation($id: String!) { deleteComment(id: $id) }",
            json!({ "id": comment_id }),
        )
        .await?;
        Ok(())
    }

    /// 从 `after` 那以后评论的变化，一页最多 100 条。第一次不写 `after`。
    pub async fn comment_changes(
        &self,
        workspace_id: &str,
        doc_id: &str,
        after: Option<&str>,
    ) -> Result<Page<CommentChange>, Error> {
        let data = self
            .graphql(
                "query($workspaceId: String!, $docId: String!, $pagination: PaginationInput!) {
                   workspace(id: $workspaceId) {
                     commentChanges(docId: $docId, pagination: $pagination) {
                       pageInfo { endCursor hasNextPage }
                       edges { node { action id commentId item } }
                     }
                   }
                 }",
                json!({ "workspaceId": workspace_id, "docId": doc_id, "pagination": { "first": 100, "after": after } }),
            )
            .await?;
        page(get(&data, &["workspace", "commentChanges"])?, |n| {
            Ok(CommentChange {
                action: string(n, "action")?,
                id: string(n, "id")?,
                comment_id: opt_string(n, "commentId"),
                item: n.get("item").cloned().unwrap_or(Value::Null),
            })
        })
    }
}

/// 一条纯文字评论的内容：每段一个段落块。AFFiNE 的评论内容是一小篇 BlockSuite 文档的快照
/// （和网页版发评论时一样：页面块 → note → 段落）。`preview` 是评论针对的那段文字，可以不写。
pub fn comment_content(paragraphs: &[&str], preview: Option<&str>) -> Result<Value, Error> {
    let paragraphs: Vec<Vec<Span<'_>>> = paragraphs.iter().map(|p| vec![Span::Text(p)]).collect();
    comment_spans(&paragraphs, preview)
}

/// 评论里一段中的一截：纯文字，或者链接。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Span<'a> {
    Text(&'a str),
    Link { text: &'a str, url: &'a str },
}

/// 和 [`comment_content`] 一样，每段可以有几截，其中可以有链接（文字上的 `link` 属性，和网页版一样）。
pub fn comment_spans(paragraphs: &[Vec<Span<'_>>], preview: Option<&str>) -> Result<Value, Error> {
    let text = |spans: &[Span<'_>]| {
        let delta: Vec<Value> = spans
            .iter()
            .filter_map(|s| match s {
                Span::Text("") | Span::Link { text: "", .. } => None,
                Span::Text(t) => Some(json!({ "insert": t })),
                Span::Link { text, url } => {
                    Some(json!({ "insert": text, "attributes": { "link": url } }))
                }
            })
            .collect();
        json!({ "$blocksuite:internal:text$": true, "delta": delta })
    };
    let block =
        |flavour: &str, version: u32, props: Value, children: Vec<Value>| -> Result<Value, Error> {
            Ok(json!({
                "type": "block",
                "id": random_id()?,
                "flavour": flavour,
                "version": version,
                "props": props,
                "children": children,
            }))
        };
    let mut paras = Vec::new();
    for p in paragraphs {
        paras.push(block(
            "affine:paragraph",
            1,
            json!({ "type": "text", "text": text(p) }),
            vec![],
        )?);
    }
    let note = block(
        "affine:note",
        1,
        json!({
            "xywh": "[0,0,800,95]",
            "background": { "dark": "#000000", "light": "#ffffff" },
            "index": "a0",
            "hidden": false,
            "displayMode": "both",
        }),
        paras,
    )?;
    let root = block("affine:page", 2, json!({ "title": text(&[]) }), vec![note])?;
    let mut content = json!({
        "snapshot": {
            "type": "page",
            "meta": { "id": random_id()?, "title": "", "createDate": now_ms(), "tags": [] },
            "blocks": root,
        },
        "mode": "page",
        "attachments": [],
    });
    if let Some(p) = preview {
        content["preview"] = json!(p);
    }
    Ok(content)
}

/// 评论内容里的纯文字（每个段落一行），给人看、和提议对照用。
pub fn comment_text(content: &Value) -> String {
    fn walk(block: &Value, out: &mut Vec<String>) {
        if let Some(delta) = block
            .get("props")
            .and_then(|p| p.get("text"))
            .and_then(|t| t.get("delta"))
            .and_then(Value::as_array)
        {
            out.push(
                delta
                    .iter()
                    .filter_map(|d| d.get("insert").and_then(Value::as_str))
                    .collect(),
            );
        }
        for c in block
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            walk(c, out);
        }
    }
    let mut lines = Vec::new();
    if let Some(root) = content.get("snapshot").and_then(|s| s.get("blocks")) {
        walk(root, &mut lines);
    }
    lines.join("\n")
}

fn random_id() -> Result<String, Error> {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz_-";
    let mut bytes = [0u8; 10];
    getrandom::fill(&mut bytes).map_err(|e| Error::Config(format!("拿不到随机数：{e}")))?;
    Ok(bytes
        .iter()
        .map(|b| ALPHABET[(*b & 63) as usize] as char)
        .collect())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn comment(n: &Value) -> Result<Comment, Error> {
    Ok(Comment {
        id: string(n, "id")?,
        content: n.get("content").cloned().unwrap_or(Value::Null),
        resolved: n.get("resolved").and_then(Value::as_bool).unwrap_or(false),
        user_id: n
            .get("user")
            .and_then(|u| u.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        user_name: n
            .get("user")
            .and_then(|u| u.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        created_at: string(n, "createdAt")?,
        updated_at: string(n, "updatedAt")?,
        replies: n
            .get("replies")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(comment).collect::<Result<_, _>>())
            .transpose()?
            .unwrap_or_default(),
    })
}

fn page<T>(conn: &Value, item: impl Fn(&Value) -> Result<T, Error>) -> Result<Page<T>, Error> {
    let items = array(conn, &["edges"])?
        .iter()
        .map(|e| item(get(e, &["node"])?))
        .collect::<Result<_, _>>()?;
    let info = conn.get("pageInfo");
    Ok(Page {
        items,
        end_cursor: info.and_then(|i| opt_string(i, "endCursor")),
        has_next_page: info
            .and_then(|i| i.get("hasNextPage"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn get<'a>(v: &'a Value, path: &[&str]) -> Result<&'a Value, Error> {
    let mut cur = v;
    for k in path {
        cur = cur
            .get(*k)
            .filter(|x| !x.is_null())
            .ok_or_else(|| Error::Protocol(format!("GraphQL 回来缺 {}", path.join("."))))?;
    }
    Ok(cur)
}

fn array<'a>(v: &'a Value, path: &[&str]) -> Result<&'a Vec<Value>, Error> {
    get(v, path)?
        .as_array()
        .ok_or_else(|| Error::Protocol(format!("GraphQL 回来的 {} 不是数组", path.join("."))))
}

fn string(v: &Value, key: &str) -> Result<String, Error> {
    opt_string(v, key).ok_or_else(|| Error::Protocol(format!("GraphQL 回来缺 {key}")))
}

fn opt_string(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_content_holds_the_text() {
        let c = comment_content(&["第一段", "", "第三段"], Some("被评论的那句")).unwrap();
        assert_eq!(comment_text(&c), "第一段\n\n第三段");
        assert_eq!(c["preview"], "被评论的那句");
        assert_eq!(c["snapshot"]["blocks"]["flavour"], "affine:page");
        assert_eq!(
            c["snapshot"]["blocks"]["children"][0]["flavour"],
            "affine:note"
        );
        let para = &c["snapshot"]["blocks"]["children"][0]["children"][0];
        assert_eq!(para["flavour"], "affine:paragraph");
        assert_eq!(para["props"]["type"], "text");
        assert_eq!(para["id"].as_str().unwrap().len(), 10);
    }

    #[test]
    fn links_in_comments() {
        let c = comment_spans(
            &[vec![
                Span::Text("在审核页处理："),
                Span::Link {
                    text: "打开",
                    url: "https://ctx.example.com/review#/proposals/p1",
                },
                Span::Text(""),
            ]],
            None,
        )
        .unwrap();
        assert_eq!(comment_text(&c), "在审核页处理：打开");
        let delta =
            &c["snapshot"]["blocks"]["children"][0]["children"][0]["props"]["text"]["delta"];
        assert_eq!(delta.as_array().unwrap().len(), 2);
        assert_eq!(
            delta[1]["attributes"]["link"],
            "https://ctx.example.com/review#/proposals/p1"
        );
        assert!(c.get("preview").is_none());
    }
}
