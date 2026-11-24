//! 远端推送的 webhook（`docs/spec.md` 6.5）：上下文仓库在 GitHub、Gitea、GitLab 上收到推送时通知 `POST /hooks/push`，
//! 服务马上和远端同步一次（拉、索引、推），不用等定时同步。
//!
//! - 只认签过名的请求，密钥是环境变量 `TCMS_WEBHOOK_SECRET`：GitHub 的 `X-Hub-Signature-256`（`sha256=` 加十六进制），
//!   Gitea、Forgejo、Gogs 的 `X-Gitea-Signature`、`X-Forgejo-Signature`、`X-Gogs-Signature`（十六进制），
//!   都是请求体的 HMAC-SHA256；GitLab 的 `X-Gitlab-Token` 是密钥本身。
//! - 不读请求体里写了什么（推到哪个分支、哪些提交）：签名对了就同步一次。同步本来就只管配好的那个分支，
//!   推到别的分支、tcms 自己推上去引起的通知，多同步一次、什么都没拉到而已。
//! - 只排进同步的队，马上回 202，不等同步做完（GitHub 等 10 秒就算超时）。几次推送赶在一次同步之前到的，合成一次。
//! - 不查 `Host`：靠签名认人，也不回任何数据，DNS 重绑定拿不到东西。

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use hmac::{Hmac, KeyInit, Mac};
use http::{HeaderMap, StatusCode};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::Writer;
use crate::api::error;

/// 请求体最多多大。GitHub 的推送通知最多 25 MB，多数几 KB；更大的回 413，等下一次定时同步。
const MAX_BODY: usize = 8 * 1024 * 1024;

/// 密钥至少多长。
pub const MIN_SECRET_LEN: usize = 16;

#[derive(Clone)]
struct HookState {
    writer: Writer,
    secret: String,
}

/// `POST /hooks/push`。`secret` 是 `TCMS_WEBHOOK_SECRET`，至少 [`MIN_SECRET_LEN`] 个字节（调用的地方查）。
/// 收到推送后由 [`Writer::spawn_sync_loop`] 起的同步循环去同步。
pub fn push_hook(writer: Writer, secret: String) -> Router {
    writer.enable_push_hook();
    Router::new()
        .route("/hooks/push", post(receive))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(HookState { writer, secret })
}

async fn receive(State(state): State<HookState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(why) = verify(&headers, &body, state.secret.as_bytes()) {
        return error(StatusCode::UNAUTHORIZED, "bad_signature", why);
    }
    match event(&headers) {
        // GitHub 建好 webhook 时发一个 ping。
        Some("ping") => (StatusCode::OK, axum::Json(json!({ "event": "ping" }))).into_response(),
        None | Some("push" | "Push Hook") => {
            state.writer.pushed();
            (StatusCode::ACCEPTED, axum::Json(json!({ "queued": true }))).into_response()
        }
        Some(other) => (
            StatusCode::OK,
            axum::Json(json!({ "ignored": other, "note": "只处理推送（push）" })),
        )
            .into_response(),
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

/// 哪一种通知。没写时当作推送（自己写的脚本、CI 调的）。
fn event(headers: &HeaderMap) -> Option<&str> {
    [
        "x-github-event",
        "x-gitea-event",
        "x-forgejo-event",
        "x-gogs-event",
        "x-gitlab-event",
    ]
    .iter()
    .find_map(|name| header(headers, name))
}

/// 核对签名。不对时回一句为什么（不带密钥、不带算出来的签名）。
fn verify(headers: &HeaderMap, body: &[u8], secret: &[u8]) -> Result<(), &'static str> {
    if let Some(v) = header(headers, "x-hub-signature-256") {
        let hex = v
            .strip_prefix("sha256=")
            .ok_or("X-Hub-Signature-256 要以 sha256= 开头")?;
        return check_hmac(secret, body, hex);
    }
    for name in [
        "x-gitea-signature",
        "x-forgejo-signature",
        "x-gogs-signature",
    ] {
        if let Some(v) = header(headers, name) {
            return check_hmac(secret, body, v);
        }
    }
    if let Some(v) = header(headers, "x-gitlab-token") {
        // 比两边的哈希，比较花的时间透露不出密钥。
        return if Sha256::digest(v.as_bytes()) == Sha256::digest(secret) {
            Ok(())
        } else {
            Err("X-Gitlab-Token 不对")
        };
    }
    Err(
        "没有签名：要 X-Hub-Signature-256（GitHub）、X-Gitea-Signature（Gitea、Forgejo）或 X-Gitlab-Token（GitLab），\
         密钥和服务的 TCMS_WEBHOOK_SECRET 一样",
    )
}

fn check_hmac(secret: &[u8], body: &[u8], hex_sig: &str) -> Result<(), &'static str> {
    let sig = hex::decode(hex_sig).map_err(|_| "签名不是十六进制")?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).map_err(|_| "密钥不能用")?;
    mac.update(body);
    // 比较花的时间和签名对了几位无关。
    mac.verify_slice(&sig).map_err(|_| "签名不对")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn github_example_signature() {
        // GitHub 文档里的例子（“Validating webhook deliveries”）。
        let secret = b"It's a Secret to Everybody";
        let body = b"Hello, World!";
        let good = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert_eq!(
            verify(&headers(&[("x-hub-signature-256", good)]), body, secret),
            Ok(())
        );
        let bad = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e18";
        assert_eq!(
            verify(&headers(&[("x-hub-signature-256", bad)]), body, secret),
            Err("签名不对")
        );
        // 少了一位、不是十六进制、没有前缀、请求体被改过。
        assert!(
            verify(
                &headers(&[("x-hub-signature-256", &good[..good.len() - 2])]),
                body,
                secret
            )
            .is_err()
        );
        assert!(
            verify(
                &headers(&[("x-hub-signature-256", "sha256=zz")]),
                body,
                secret
            )
            .is_err()
        );
        assert!(
            verify(
                &headers(&[("x-hub-signature-256", &good[7..])]),
                body,
                secret
            )
            .is_err()
        );
        assert!(
            verify(
                &headers(&[("x-hub-signature-256", good)]),
                b"Hello, World?",
                secret
            )
            .is_err()
        );
        // Gitea 的签名没有前缀，算法一样。
        assert_eq!(
            verify(&headers(&[("x-gitea-signature", &good[7..])]), body, secret),
            Ok(())
        );
    }

    #[test]
    fn gitlab_token_and_missing() {
        let secret = b"0123456789abcdef-secret";
        assert_eq!(
            verify(
                &headers(&[("x-gitlab-token", "0123456789abcdef-secret")]),
                b"{}",
                secret
            ),
            Ok(())
        );
        assert!(
            verify(
                &headers(&[("x-gitlab-token", "0123456789abcdef")]),
                b"{}",
                secret
            )
            .is_err()
        );
        assert!(verify(&HeaderMap::new(), b"{}", secret).is_err());
    }

    #[test]
    fn events() {
        assert_eq!(event(&HeaderMap::new()), None);
        assert_eq!(event(&headers(&[("x-github-event", "push")])), Some("push"));
        assert_eq!(
            event(&headers(&[("x-gitlab-event", "Push Hook")])),
            Some("Push Hook")
        );
    }
}
