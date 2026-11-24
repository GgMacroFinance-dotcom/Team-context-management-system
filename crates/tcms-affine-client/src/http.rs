//! 一次 HTTP 请求一个连接（hyper 的单连接客户端）。只有登录和 GraphQL 用，次数少，不值得留连接池。

use std::time::Duration;

use bytes::Bytes;
use http::{Method, Request, header};
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;

use crate::Error;
use crate::net::{Endpoint, Tls, connect};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// 回来的内容最多这么大（GraphQL 列评论、文档时够用）。
const MAX_BODY: usize = 32 << 20;

pub(crate) struct Response {
    pub status: u16,
    /// 每个 `Set-Cookie` 头的原样内容。
    pub set_cookies: Vec<String>,
    pub body: Bytes,
}

impl Response {
    pub(crate) fn json(&self) -> Result<serde_json::Value, Error> {
        serde_json::from_slice(&self.body).map_err(|e| {
            Error::Protocol(format!(
                "回来的不是 JSON（HTTP {}）：{e}：{}",
                self.status,
                preview(&self.body)
            ))
        })
    }
}

pub(crate) async fn request(
    endpoint: &Endpoint,
    tls: &Tls,
    method: Method,
    path: &str,
    headers: &[(header::HeaderName, String)],
    body: Option<&serde_json::Value>,
) -> Result<Response, Error> {
    tokio::time::timeout(
        REQUEST_TIMEOUT,
        send(endpoint, tls, method, path, headers, body),
    )
    .await
    .map_err(|_| Error::Network(format!("请求 {endpoint}{path} 超时")))?
}

async fn send(
    endpoint: &Endpoint,
    tls: &Tls,
    method: Method,
    path: &str,
    headers: &[(header::HeaderName, String)],
    body: Option<&serde_json::Value>,
) -> Result<Response, Error> {
    let stream = connect(endpoint, tls).await?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| Error::Network(format!("和 {endpoint} 的 HTTP 握手失败：{e}")))?;
    tokio::spawn(async move {
        // 请求做完以后连接自己关掉；出错在下面的 send_request 里已经看得到。
        let _ = conn.await;
    });

    let mut builder = Request::builder()
        .method(method)
        .uri(endpoint.path(path))
        .header(header::HOST, endpoint.authority())
        .header(header::ACCEPT, "application/json")
        .header(header::CONNECTION, "close");
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    let payload = match body {
        Some(v) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Bytes::from(serde_json::to_vec(v).map_err(|e| Error::Protocol(e.to_string()))?)
        }
        None => Bytes::new(),
    };
    let req = builder
        .body(Full::new(payload))
        .map_err(|e| Error::Protocol(format!("请求不对：{e}")))?;
    let res = sender
        .send_request(req)
        .await
        .map_err(|e| Error::Network(format!("请求 {endpoint}{path} 失败：{e}")))?;
    let status = res.status().as_u16();
    let set_cookies = res
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_owned))
        .collect();
    let body = Limited::new(res.into_body(), MAX_BODY)
        .collect()
        .await
        .map_err(|e| Error::Network(format!("读 {endpoint}{path} 的回复失败：{e}")))?
        .to_bytes();
    Ok(Response {
        status,
        set_cookies,
        body,
    })
}

/// 错误信息里放一小段回复，不放整段。
pub(crate) fn preview(body: &[u8]) -> String {
    let s = String::from_utf8_lossy(body);
    let mut out: String = s.chars().take(200).collect();
    if s.chars().count() > 200 {
        out.push('…');
    }
    out
}
