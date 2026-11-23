//! MCP 协议这一层：stdio 和 Streamable HTTP 两种接法，身份都来自令牌。
//!
//! - stdio：给同机的 agent。令牌从启动时的环境变量来，每次调用都重新核对（吊销了马上失效）。
//! - HTTP：`/mcp`，每个请求带 `Authorization: Bearer <令牌>`；没带或无效时直接回 401，进不了 MCP。
//!   服务无状态，不发会话 id。默认只接受本机的 `Host`（防 DNS 重绑定），对外提供时在 `allowed_hosts` 里加上域名。

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use http::{HeaderValue, StatusCode, header};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, ServerHandler, ServiceExt};
use tcms_store::Identity;

use crate::{Error, Live, tools};

#[derive(Clone)]
enum Auth {
    /// stdio：启动时给的令牌明文。
    Secret(Arc<str>),
    /// HTTP：中间件已经核对过令牌，身份放在请求的 extensions 里。
    Http,
}

/// 每个 MCP 会话（stdio）或每个请求（HTTP）的处理者。
#[derive(Clone)]
pub struct ContextServer {
    live: Arc<Live>,
    auth: Auth,
}

impl ContextServer {
    /// stdio 用：令牌明文（`TCMS_TOKEN`）。
    pub fn with_secret(live: Arc<Live>, secret: impl Into<Arc<str>>) -> Self {
        Self {
            live,
            auth: Auth::Secret(secret.into()),
        }
    }

    async fn identity(&self, ctx: &RequestContext<RoleServer>) -> Result<Identity, ErrorData> {
        match &self.auth {
            Auth::Secret(secret) => self
                .live
                .store()
                .identify(secret)
                .await
                .map_err(|e| ErrorData::internal_error(format!("核对令牌时出错：{e}"), None))?
                .ok_or_else(invalid_token),
            Auth::Http => ctx
                .extensions
                .get::<http::request::Parts>()
                .and_then(|p| p.extensions.get::<Arc<Identity>>())
                .map(|i| (**i).clone())
                .ok_or_else(invalid_token),
        }
    }
}

fn invalid_token() -> ErrorData {
    ErrorData::invalid_request("令牌无效、已吊销或已过期", None)
}

impl ServerHandler for ContextServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("tcms", env!("CARGO_PKG_VERSION")))
            .with_instructions(tools::INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.identity(&context).await?;
        Ok(ListToolsResult::with_all_items(tools::list()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let who = self.identity(&context).await?;
        let args = request.arguments.unwrap_or_default();
        match tools::call(&self.live, &who, &request.name, args).await {
            Some(out) => Ok(tools::to_result(out).into()),
            None => Err(ErrorData::invalid_params(
                format!("没有这个工具：{}", request.name),
                None,
            )),
        }
    }
}

/// stdio：从标准输入读请求、往标准输出写回答，直到对方断开。标准输出只放协议消息，提示一律写标准错误。
pub async fn serve_stdio(live: Arc<Live>, secret: String) -> Result<(), Error> {
    let running = ContextServer::with_secret(live, secret)
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| Error::Mcp(e.to_string()))?;
    running
        .waiting()
        .await
        .map_err(|e| Error::Join(e.to_string()))?;
    Ok(())
}

/// HTTP：在 `listener` 上提供 `app`（[`router`]，可以再合上别的路由，比如审核页）。
/// `shutdown` 完成后不再接新连接，等手上的请求做完再返回。
pub async fn serve_http(
    listener: tokio::net::TcpListener,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
}

/// HTTP 服务的设置。
#[derive(Debug, Clone, Default)]
pub struct HttpOptions {
    /// 除了本机（`localhost`、`127.0.0.1`、`::1`）以外还接受的 `Host`（域名、IP，或带端口）。
    pub allowed_hosts: Vec<String>,
}

/// `/mcp`（要令牌）和 `/healthz`（不要）。
pub fn router(live: Arc<Live>, options: HttpOptions) -> Router {
    let mut config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true);
    config.allowed_hosts.extend(options.allowed_hosts);
    let for_service = live.clone();
    let service = StreamableHttpService::new(
        move || {
            Ok(ContextServer {
                live: for_service.clone(),
                auth: Auth::Http,
            })
        },
        Arc::new(NeverSessionManager::default()),
        config,
    );
    Router::new()
        .route_service("/mcp", service)
        .route_layer(middleware::from_fn_with_state(live, require_token))
        .route("/healthz", get(|| async { "ok" }))
}

/// 从 `Authorization: Bearer <令牌>` 里取出令牌。
pub fn bearer(headers: &http::HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

async fn require_token(State(live): State<Arc<Live>>, mut req: Request, next: Next) -> Response {
    let Some(secret) = bearer(req.headers()) else {
        return unauthorized("缺少令牌（Authorization: Bearer <令牌>）");
    };
    match live.store().identify(secret).await {
        Ok(Some(who)) => {
            req.extensions_mut().insert(Arc::new(who));
            next.run(req).await
        }
        Ok(None) => unauthorized("令牌无效、已吊销或已过期"),
        Err(e) => {
            eprintln!("核对令牌时出错：{e}");
            (StatusCode::SERVICE_UNAVAILABLE, "暂时核对不了令牌").into_response()
        }
    }
}

fn unauthorized(msg: &'static str) -> Response {
    let mut r = (StatusCode::UNAUTHORIZED, msg).into_response();
    r.headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_parsing() {
        let mut h = http::HeaderMap::new();
        assert_eq!(bearer(&h), None);
        h.insert(header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(bearer(&h), Some("abc"));
        h.insert(header::AUTHORIZATION, "bearer  abc ".parse().unwrap());
        assert_eq!(bearer(&h), Some("abc"));
        h.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert_eq!(bearer(&h), None);
        h.insert(header::AUTHORIZATION, "Bearer ".parse().unwrap());
        assert_eq!(bearer(&h), None);
    }
}
