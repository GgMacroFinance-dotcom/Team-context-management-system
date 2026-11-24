//! 审核页（`docs/spec.md` 6.4，`docs/architecture.md` 第 7 节）：和 `/mcp` 放在同一个 HTTP 服务里。
//!
//! - `/review`：一个网页（HTML、JS、CSS 都编进程序里，不用另外构建）。
//! - `/api/...`：网页用的 JSON 接口，每个请求带 `Authorization: Bearer <审核的人的令牌>`。agent 的令牌在这里不认，
//!   审核的人的令牌在 `/mcp` 也不认，agent 批准不了自己的提议。
//! - 批准、改单条要写 git：放在一个专门的线程里排队做（[`Writer`]），和命令行的 `tcms accept` 排在同一个提交队列里。
//!
//! 第一版审核的人能看、能处理所有范围。

mod api;
mod writer;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use http::{HeaderValue, StatusCode, header};
use tcms_mcp::{Live, bearer};

pub use writer::{Contact, RemoteStatus, Writer};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] tcms_store::Error),
    #[error("起写入线程：{0}")]
    Thread(String),
}

/// 审核页的设置。
#[derive(Debug, Clone, Default)]
pub struct ReviewOptions {
    /// 除了本机以外还接受的 `Host`（和 `/mcp` 的一样，防 DNS 重绑定）。
    pub allowed_hosts: Vec<String>,
    /// 审核页里批准、改单条时 git 提交者的邮箱（名字是审核的人的名字）。空的时候是 `tcms@localhost`。
    pub author_email: String,
}

#[derive(Clone)]
struct AppState {
    live: Arc<Live>,
    writer: Writer,
    author_email: String,
}

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_JS: &str = include_str!("../assets/app.js");
const APP_CSS: &str = include_str!("../assets/app.css");

/// 网页只从本服务取脚本、样式和数据；不许嵌进别的页面（以后 I.P. 控制台要嵌时再放开）。
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// `/review`（网页）和 `/api/...`（要审核的人的令牌）。`/` 跳到 `/review`。
pub fn router(live: Arc<Live>, writer: Writer, options: ReviewOptions) -> Router {
    let author_email = match options.author_email.trim() {
        "" => tcms_git::Author::default().email,
        e => e.to_owned(),
    };
    let state = AppState {
        live,
        writer,
        author_email,
    };
    let api = Router::new()
        .route("/api/me", get(api::me))
        .route("/api/proposals", get(api::proposals))
        .route("/api/proposals/{id}", get(api::proposal))
        .route("/api/proposals/{id}/accept", post(api::accept))
        .route("/api/proposals/{id}/reject", post(api::reject))
        .route("/api/due", get(api::due))
        .route("/api/items", get(api::items))
        .route("/api/items/{id}", get(api::item))
        .route("/api/items/{id}/edit", post(api::edit))
        .route("/api/items/{id}/promote", post(api::promote))
        .route("/api/promotions", get(api::promotions))
        .route("/api/remote", get(api::remote))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_reviewer,
        ))
        .with_state(state);
    let pages = Router::new()
        .route("/", get(|| async { Redirect::to("/review") }))
        .route(
            "/review",
            get(|| async { asset(INDEX_HTML, "text/html; charset=utf-8") }),
        )
        .route(
            "/review/app.js",
            get(|| async { asset(APP_JS, "text/javascript; charset=utf-8") }),
        )
        .route(
            "/review/app.css",
            get(|| async { asset(APP_CSS, "text/css; charset=utf-8") }),
        );
    let hosts = Arc::new(allowed_hosts(&options.allowed_hosts));
    pages
        .merge(api)
        .layer(middleware::from_fn(security_headers))
        .layer(middleware::from_fn_with_state(hosts, check_host))
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    let mut r = body.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

async fn security_headers(req: Request, next: Next) -> Response {
    let mut r = next.run(req).await;
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    r
}

async fn require_reviewer(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let Some(secret) = bearer(req.headers()) else {
        return api::error(
            StatusCode::UNAUTHORIZED,
            "login",
            "要先登录（审核的人的令牌）",
        );
    };
    match state.live.store().identify_reviewer(secret).await {
        Ok(Some(who)) => {
            req.extensions_mut().insert(who);
            next.run(req).await
        }
        Ok(None) => api::error(
            StatusCode::UNAUTHORIZED,
            "login",
            "令牌无效、已吊销、已过期，或者不是审核的人的令牌（tcms token add --reviewer 发的）",
        ),
        Err(e) => {
            eprintln!("核对令牌时出错：{e}");
            api::error(
                StatusCode::SERVICE_UNAVAILABLE,
                "internal",
                "暂时核对不了令牌",
            )
        }
    }
}

/// 一个允许的 `Host`：主机名（小写，IPv6 不带方括号），和可选的端口。
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostRule {
    host: String,
    port: Option<u16>,
}

fn parse_host(value: &str) -> Option<HostRule> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }
    // 不带方括号的 IPv6（比如 ::1）：整个是主机名。
    if value.matches(':').count() > 1 && !value.starts_with('[') {
        return Some(HostRule {
            host: value,
            port: None,
        });
    }
    let authority: http::uri::Authority = value.parse().ok()?;
    Some(HostRule {
        host: authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned(),
        port: authority.port_u16(),
    })
}

fn allowed_hosts(extra: &[String]) -> Vec<HostRule> {
    ["localhost", "127.0.0.1", "::1"]
        .iter()
        .map(|h| (*h).to_owned())
        .chain(extra.iter().cloned())
        .filter_map(|h| parse_host(&h))
        .collect()
}

fn host_allowed(host: &str, rules: &[HostRule]) -> bool {
    let Some(h) = parse_host(host) else {
        return false;
    };
    rules
        .iter()
        .any(|r| r.host == h.host && r.port.is_none_or(|p| h.port == Some(p)))
}

async fn check_host(State(rules): State<Arc<Vec<HostRule>>>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|h| host_allowed(h, &rules));
    if ok {
        next.run(req).await
    } else {
        (
            StatusCode::FORBIDDEN,
            "Host 不在允许的列表里（对外提供时用 --allowed-host 写上域名）",
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        let rules = allowed_hosts(&["ctx.example.com".into(), "10.0.0.5:8080".into()]);
        for ok in [
            "localhost:7341",
            "LOCALHOST",
            "127.0.0.1:7341",
            "[::1]:7341",
            "ctx.example.com",
            "ctx.example.com:443",
            "10.0.0.5:8080",
        ] {
            assert!(host_allowed(ok, &rules), "{ok}");
        }
        for bad in [
            "evil.com",
            "10.0.0.5:9090",
            "10.0.0.5",
            "ctx.example.com.evil.com",
            "",
        ] {
            assert!(!host_allowed(bad, &rules), "{bad}");
        }
    }
}
