//! 和一个 AFFiNE 服务的连接：账号、会话、GraphQL。同步协议在 [`crate::sync`]。
//!
//! 会话用的是 AFFiNE 网页版那种 cookie（`affine_session`、`affine_user_id`）：邮箱密码登录一次，
//! 以后 HTTP 请求和 WebSocket 握手都带上。0.27.5-beta.3 的会话 15 天过期；这边 7 天重新登录一次，
//! 中间服务端说没登录（比如会话被管理员撤了）也马上重新登录。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use http::Method;
use http::header::{self, HeaderName};
use serde_json::{Value, json};
use tokio::sync::{Mutex, OnceCell};
use tokio::time::Instant;

use crate::Error;
use crate::http::{Response, preview, request};
use crate::net::{Endpoint, Tls};

const SESSION_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
pub(crate) const VERSION_HEADER: HeaderName = HeaderName::from_static("x-affine-version");

/// 登录用的账号。
#[derive(Clone)]
pub struct Credentials {
    pub email: String,
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("email", &self.email)
            .field("password", &"…")
            .finish()
    }
}

/// 连哪个 AFFiNE、用哪个账号。
#[derive(Clone, Debug)]
pub struct Config {
    /// `http(s)://主机[:端口][/前缀]`。
    pub url: String,
    pub credentials: Credentials,
    /// 加入工作区时报的客户端版本。不写就用服务端自己报的版本（`/info` 的 `compatibility`），
    /// 和这个服务端自带的网页版报的一样。
    pub client_version: Option<String>,
    /// 自托管的 AFFiNE 用自己的 CA 时，PEM 格式的 CA 证书文件。
    pub extra_ca: Option<PathBuf>,
}

/// 服务端的 `/info`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    /// 比如 `0.27.5-beta.3`。
    pub version: String,
    /// `selfhosted`、`affine`……
    pub kind: String,
}

#[derive(Clone)]
pub(crate) struct Session {
    pub cookie: String,
    pub user_id: String,
    signed_in: Instant,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("user_id", &self.user_id)
            .finish_non_exhaustive()
    }
}

/// 一个 AFFiNE 服务。可以克隆，克隆出来的共用一个会话。
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    endpoint: Endpoint,
    tls: Tls,
    credentials: Credentials,
    client_version: OnceCell<String>,
    session: Mutex<Option<Session>>,
}

impl Client {
    pub fn new(config: Config) -> Result<Self, Error> {
        let endpoint = Endpoint::parse(&config.url)?;
        let tls = Tls::new(config.extra_ca.as_deref())?;
        let client_version = OnceCell::new();
        if let Some(v) = config.client_version {
            client_version
                .set(v)
                .map_err(|_| Error::Config("客户端版本设了两次".into()))?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                endpoint,
                tls,
                credentials: config.credentials,
                client_version,
                session: Mutex::new(None),
            }),
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.inner.endpoint
    }

    pub(crate) fn tls(&self) -> &Tls {
        &self.inner.tls
    }

    /// 服务端的版本和类型，不用登录。
    pub async fn server_info(&self) -> Result<ServerInfo, Error> {
        let res = self.http(Method::GET, "/info", &[], None).await?;
        if res.status != 200 {
            return Err(Error::Network(format!(
                "{}/info 回了 HTTP {}：{}",
                self.inner.endpoint,
                res.status,
                preview(&res.body)
            )));
        }
        let v = res.json()?;
        let field = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        Ok(ServerInfo {
            version: field("compatibility")
                .ok_or_else(|| Error::Protocol(format!("/info 里没有版本：{v}")))?,
            kind: field("type").unwrap_or_default(),
        })
    }

    /// 加入工作区时报的客户端版本。
    pub async fn client_version(&self) -> Result<String, Error> {
        self.inner
            .client_version
            .get_or_try_init(|| async { Ok(self.server_info().await?.version) })
            .await
            .cloned()
    }

    /// 这个账号在 AFFiNE 里的用户 id。
    pub async fn user_id(&self) -> Result<String, Error> {
        Ok(self.session().await?.user_id)
    }

    /// 现在的会话，没有或者太旧就登录一次。
    pub(crate) async fn session(&self) -> Result<Session, Error> {
        let mut guard = self.inner.session.lock().await;
        if let Some(s) = guard.as_ref()
            && s.signed_in.elapsed() < SESSION_MAX_AGE
        {
            return Ok(s.clone());
        }
        let s = self.sign_in().await?;
        *guard = Some(s.clone());
        Ok(s)
    }

    /// 服务端说没登录：这个会话不要了，下次重新登录。别人已经换了新的就不动。
    pub(crate) async fn drop_session(&self, stale: &Session) {
        let mut guard = self.inner.session.lock().await;
        if guard.as_ref().is_some_and(|s| s.cookie == stale.cookie) {
            *guard = None;
        }
    }

    async fn sign_in(&self) -> Result<Session, Error> {
        let creds = &self.inner.credentials;
        let version = self.client_version().await?;
        let body = json!({ "email": creds.email, "password": creds.password });
        let res = self
            .http(
                Method::POST,
                "/api/auth/sign-in",
                &[(VERSION_HEADER, version)],
                Some(&body),
            )
            .await?;
        if res.status != 200 {
            let (name, message) = error_of(&res);
            // 429：登录太频繁（AFFiNE 对登录限流），过一会儿再试。
            return Err(if matches!(res.status, 400 | 401 | 403) {
                Error::Auth(format!("{name}：{message}"))
            } else {
                Error::Remote { name, message }
            });
        }
        let mut session = None;
        let mut user = None;
        for c in &res.set_cookies {
            let pair = c.split(';').next().unwrap_or_default().trim();
            if let Some(v) = pair.strip_prefix("affine_session=") {
                session = Some(v.to_owned());
            } else if let Some(v) = pair.strip_prefix("affine_user_id=") {
                user = Some(v.to_owned());
            }
        }
        let body = res.json()?;
        let user_id = user
            .or_else(|| body.get("id").and_then(Value::as_str).map(str::to_owned))
            .ok_or_else(|| Error::Protocol("登录回来没有用户 id".into()))?;
        let session = session.ok_or_else(|| {
            Error::Auth("登录回来没有 affine_session（服务端可能要求用别的方式登录）".into())
        })?;
        Ok(Session {
            cookie: format!("affine_session={session}; affine_user_id={user_id}"),
            user_id,
            signed_in: Instant::now(),
        })
    }

    pub(crate) async fn http(
        &self,
        method: Method,
        path: &str,
        headers: &[(HeaderName, String)],
        body: Option<&Value>,
    ) -> Result<Response, Error> {
        request(
            &self.inner.endpoint,
            &self.inner.tls,
            method,
            path,
            headers,
            body,
        )
        .await
    }

    /// 带着会话的请求头（cookie 和客户端版本）。
    pub(crate) async fn auth_headers(
        &self,
        session: &Session,
    ) -> Result<Vec<(HeaderName, String)>, Error> {
        Ok(vec![
            (header::COOKIE, session.cookie.clone()),
            (VERSION_HEADER, self.client_version().await?),
        ])
    }

    /// 发一个 GraphQL 请求，返回 `data`。服务端说没登录时重新登录再试一次。
    pub async fn graphql(&self, query: &str, variables: Value) -> Result<Value, Error> {
        let body = json!({ "query": query, "variables": variables });
        let mut retried = false;
        loop {
            let session = self.session().await?;
            let headers = self.auth_headers(&session).await?;
            let res = self
                .http(Method::POST, "/graphql", &headers, Some(&body))
                .await?;
            let v = res.json()?;
            let first_error = v
                .get("errors")
                .and_then(Value::as_array)
                .and_then(|e| e.first());
            if let Some(err) = first_error {
                let ext = err.get("extensions").cloned().unwrap_or(Value::Null);
                let name = ext
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("GRAPHQL_ERROR")
                    .to_owned();
                let message = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let unauthenticated = name == "AUTHENTICATION_REQUIRED"
                    || ext.get("status").and_then(Value::as_u64) == Some(401);
                if unauthenticated && !retried {
                    self.drop_session(&session).await;
                    retried = true;
                    continue;
                }
                return Err(if unauthenticated {
                    Error::Auth(message)
                } else {
                    Error::Remote { name, message }
                });
            }
            if res.status != 200 {
                let (name, message) = error_of(&res);
                return Err(Error::Remote { name, message });
            }
            return v.get("data").cloned().ok_or_else(|| {
                Error::Protocol(format!("GraphQL 回来没有 data：{}", preview(&res.body)))
            });
        }
    }
}

/// AFFiNE 的错误回复：`{"name": "...", "message": "..."}`。
fn error_of(res: &Response) -> (String, String) {
    match res.json() {
        Ok(v) => (
            v.get("name")
                .and_then(Value::as_str)
                .unwrap_or("HTTP_ERROR")
                .to_owned(),
            v.get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("HTTP {}", res.status)),
        ),
        Err(_) => (
            "HTTP_ERROR".to_owned(),
            format!("HTTP {}：{}", res.status, preview(&res.body)),
        ),
    }
}
