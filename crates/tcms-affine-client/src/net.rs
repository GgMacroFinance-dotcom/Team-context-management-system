//! 地址和连接：解析 AFFiNE 服务的地址，连 TCP，`https` 时在上面接 TLS。
//!
//! HTTP 请求和 WebSocket 都走这里拿到的连接。TLS 用 rustls（ring），根证书是 Mozilla 的那份（`webpki-roots`），
//! 自托管的 AFFiNE 用自己的 CA 时可以再加一个 PEM 文件。不支持走 HTTP 代理。

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};

use crate::Error;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// AFFiNE 服务的地址：`http(s)://主机[:端口][/前缀]`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    tls: bool,
    host: String,
    port: u16,
    /// 不带结尾的 `/`，没有前缀时是空的。
    prefix: String,
}

impl Endpoint {
    pub fn parse(url: &str) -> Result<Self, Error> {
        let bad = || Error::Config(format!("AFFiNE 的地址不对：{url}"));
        let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(bad());
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority.is_empty() || authority.contains('@') {
            return Err(bad());
        }
        let default_port = if tls { 443 } else { 80 };
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let end = v6.find(']').ok_or_else(bad)?;
            let host = &v6[..end];
            let port = match &v6[end + 1..] {
                "" => default_port,
                p => p
                    .strip_prefix(':')
                    .and_then(|p| p.parse().ok())
                    .ok_or_else(bad)?,
            };
            (host.to_owned(), port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_owned(), p.parse().map_err(|_| bad())?),
                None => (authority.to_owned(), default_port),
            }
        };
        if host.is_empty() || path.contains(['?', '#']) {
            return Err(bad());
        }
        Ok(Self {
            tls,
            host,
            port,
            prefix: path.trim_end_matches('/').to_owned(),
        })
    }

    /// `Host` 头里写的：默认端口不写。
    pub(crate) fn authority(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == if self.tls { 443 } else { 80 } {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }

    /// 服务上的一个路径（`path` 以 `/` 开头）。
    pub(crate) fn path(&self, path: &str) -> String {
        format!("{}{path}", self.prefix)
    }

    /// WebSocket 的地址。
    pub(crate) fn ws_url(&self, path_and_query: &str) -> String {
        let scheme = if self.tls { "wss" } else { "ws" };
        format!(
            "{scheme}://{}{}",
            self.authority(),
            self.path(path_and_query)
        )
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scheme = if self.tls { "https" } else { "http" };
        write!(f, "{scheme}://{}{}", self.authority(), self.prefix)
    }
}

/// TLS 的设置，建一次，HTTP 和 WebSocket 共用。
#[derive(Clone)]
pub(crate) struct Tls {
    connector: TlsConnector,
}

impl Tls {
    pub(crate) fn new(extra_ca: Option<&Path>) -> Result<Self, Error> {
        let mut roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        if let Some(path) = extra_ca {
            let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(path)
                .and_then(|it| it.collect())
                .map_err(|e| Error::Config(format!("读不了 CA 文件 {}：{e}", path.display())))?;
            if certs.is_empty() {
                return Err(Error::Config(format!(
                    "CA 文件 {} 里没有证书",
                    path.display()
                )));
            }
            for cert in certs {
                roots.add(cert).map_err(|e| {
                    Error::Config(format!("CA 文件 {} 里的证书不对：{e}", path.display()))
                })?;
            }
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::Config(format!("TLS 设置不对：{e}")))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            connector: TlsConnector::from(Arc::new(config)),
        })
    }
}

/// 连上的连接：明文的 TCP，或者上面接了 TLS。
pub(crate) enum Stream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

pub(crate) async fn connect(endpoint: &Endpoint, tls: &Tls) -> Result<Stream, Error> {
    let tcp = tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((endpoint.host.as_str(), endpoint.port)),
    )
    .await
    .map_err(|_| Error::Network(format!("连 {endpoint} 超时")))?
    .map_err(|e| Error::Network(format!("连不上 {endpoint}：{e}")))?;
    tcp.set_nodelay(true).ok();
    if !endpoint.tls {
        return Ok(Stream::Plain(tcp));
    }
    let name = ServerName::try_from(endpoint.host.clone())
        .map_err(|_| Error::Config(format!("主机名不对：{}", endpoint.host)))?;
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, tls.connector.connect(name, tcp))
        .await
        .map_err(|_| Error::Network(format!("和 {endpoint} 握手超时")))?
        .map_err(|e| Error::Network(format!("和 {endpoint} 的 TLS 握手失败：{e}")))?;
    Ok(Stream::Tls(Box::new(stream)))
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_parsed() {
        let e = Endpoint::parse("http://127.0.0.1:3010").unwrap();
        assert_eq!(e.authority(), "127.0.0.1:3010");
        assert_eq!(e.path("/graphql"), "/graphql");
        assert_eq!(
            e.ws_url("/socket.io/?EIO=4"),
            "ws://127.0.0.1:3010/socket.io/?EIO=4"
        );

        let e = Endpoint::parse("https://affine.example.com/").unwrap();
        assert_eq!(e.authority(), "affine.example.com");
        assert_eq!(e.to_string(), "https://affine.example.com");

        let e = Endpoint::parse("https://example.com:8443/affine/").unwrap();
        assert_eq!(e.authority(), "example.com:8443");
        assert_eq!(e.path("/graphql"), "/affine/graphql");
        assert_eq!(
            e.ws_url("/socket.io/"),
            "wss://example.com:8443/affine/socket.io/"
        );

        let e = Endpoint::parse("http://[::1]:3010").unwrap();
        assert_eq!(e.authority(), "[::1]:3010");

        for bad in [
            "affine.example.com",
            "ftp://x",
            "http://",
            "http://u:p@x",
            "http://x:y",
            "http://x/?a",
        ] {
            assert!(Endpoint::parse(bad).is_err(), "{bad}");
        }
    }
}
