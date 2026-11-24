//! 对着本机起的一个假 AFFiNE（只会登录、`/info` 和几个 socket.io 事件）测连接层：握手、回执、心跳、广播、
//! 断线重连以后重新加入。真的 AFFiNE 在 `tests/affine.rs`。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

use crate::*;

#[derive(Default)]
struct Seen {
    /// 每条 WebSocket 连接收到的事件：（第几条连接，事件名，内容）。
    events: Vec<(usize, String, Value)>,
    pongs: usize,
    connections: usize,
    cookies: Vec<String>,
}

struct Fake {
    url: String,
    seen: Arc<Mutex<Seen>>,
    /// 叫一下就把现在的 WebSocket 连接直接断掉（不发关闭帧）。
    kill: Arc<Notify>,
}

impl Fake {
    async fn start(password: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Seen::default()));
        let kill = Arc::new(Notify::new());
        let (s, k) = (seen.clone(), kill.clone());
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(handle(stream, password, s.clone(), k.clone()));
            }
        });
        Self { url, seen, kill }
    }

    fn client(&self, password: &str) -> Client {
        Client::new(Config {
            url: self.url.clone(),
            credentials: Credentials {
                email: "bot@example.test".into(),
                password: password.into(),
            },
            client_version: None,
            extra_ca: None,
        })
        .unwrap()
    }

    fn events(&self, name: &str) -> Vec<(usize, Value)> {
        self.seen
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|(_, n, _)| n == name)
            .map(|(c, _, v)| (*c, v.clone()))
            .collect()
    }
}

// tungstenite 的握手回调签名就是这样，错的那一边是整个 HTTP 回复。
#[allow(clippy::result_large_err)]
async fn handle(mut stream: TcpStream, password: &str, seen: Arc<Mutex<Seen>>, kill: Arc<Notify>) {
    let mut head = [0u8; 64];
    let n = stream.peek(&mut head).await.unwrap();
    let head = String::from_utf8_lossy(&head[..n]).to_string();
    if head.starts_with("GET /info") {
        read_request(&mut stream).await;
        respond(
            &mut stream,
            200,
            &[],
            &json!({"compatibility": "0.27.5-fake", "type": "selfhosted"}),
        )
        .await;
        return;
    }
    if head.starts_with("POST /api/auth/sign-in") {
        let body = read_request(&mut stream).await;
        let creds: Value = serde_json::from_slice(&body).unwrap();
        if creds["password"] == password {
            let cookies = [
                "affine_session=s1; Path=/; HttpOnly",
                "affine_user_id=u1; Path=/",
            ];
            respond(&mut stream, 200, &cookies, &json!({"id": "u1"})).await;
        } else {
            respond(
                &mut stream,
                400,
                &[],
                &json!({"name": "WRONG_SIGN_IN_CREDENTIALS", "message": "Wrong user email or password"}),
            )
            .await;
        }
        return;
    }

    let mut cookie = String::new();
    let mut ws = tokio_tungstenite::accept_hdr_async(stream, |req: &Request, res: Response| {
        cookie = req
            .headers()
            .get("cookie")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        Ok(res)
    })
    .await
    .unwrap();
    let conn = {
        let mut s = seen.lock().unwrap();
        s.connections += 1;
        s.cookies.push(cookie);
        s.connections
    };
    send_text(
        &mut ws,
        r#"0{"sid":"e","upgrades":[],"pingInterval":200,"pingTimeout":200,"maxPayload":1000000}"#
            .into(),
    )
    .await;
    match ws.next().await {
        Some(Ok(Message::Text(t))) if t.as_str() == "40{}" => {}
        other => panic!("应该先连默认命名空间：{other:?}"),
    }
    send_text(&mut ws, r#"40{"sid":"s"}"#.into()).await;
    let mut ping = tokio::time::interval(Duration::from_millis(100));
    let mut broadcasted = false;
    loop {
        tokio::select! {
            _ = kill.notified() => return,
            _ = ping.tick() => {
                if !send_text(&mut ws, "2".into()).await { return; }
            }
            msg = ws.next() => {
                let Some(Ok(Message::Text(t))) = msg else { return };
                let t = t.as_str();
                if t == "3" {
                    seen.lock().unwrap().pongs += 1;
                    continue;
                }
                let Some(rest) = t.strip_prefix("42") else { continue };
                let digits = rest.find('[').unwrap();
                let id = &rest[..digits];
                let v: Value = serde_json::from_str(&rest[digits..]).unwrap();
                let (name, data) = (v[0].as_str().unwrap().to_owned(), v[1].clone());
                seen.lock().unwrap().events.push((conn, name.clone(), data.clone()));
                let reply = match name.as_str() {
                    "space:join-batch" => json!({"data": {"clientId": "s", "success": true, "protocolVersion": 1}}),
                    "space:load-doc" if data["docId"] == "missing" => {
                        json!({"error": {"name": "DOC_NOT_FOUND", "message": "Doc missing not found"}})
                    }
                    "space:load-doc" => json!({"data": {"missing": BASE64.encode([1u8, 2]), "state": BASE64.encode([3u8]), "timestamp": 7}}),
                    "space:push-doc-update" => json!({"data": {"accepted": true, "timestamp": 9, "created": false}}),
                    "space:load-doc-timestamps" => json!({"data": {"d1": 5, "d2": 6}}),
                    _ => json!({"error": {"name": "UNKNOWN", "message": name}}),
                };
                if !send_text(&mut ws, format!("43{id}{}", json!([reply]))).await { return; }
                if name == "space:join-batch" && !broadcasted {
                    broadcasted = true;
                    let ev = json!(["space:broadcast-doc-updates", {
                        "spaceType": "workspace", "spaceId": "w", "docId": "d1",
                        "updates": [BASE64.encode([4u8, 5])], "timestamp": 10, "editor": "someone"
                    }]);
                    if !send_text(&mut ws, format!("42{ev}")).await { return; }
                }
            }
        }
    }
}

async fn send_text(ws: &mut tokio_tungstenite::WebSocketStream<TcpStream>, t: String) -> bool {
    ws.send(Message::text(t)).await.is_ok()
}

async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await.unwrap();
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map(|v| v.trim().parse().unwrap())
                .unwrap_or(0);
            while buf.len() < end + 4 + len {
                let n = stream.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
            }
            return buf[end + 4..end + 4 + len].to_vec();
        }
        if n == 0 {
            return Vec::new();
        }
    }
}

async fn respond(stream: &mut TcpStream, status: u16, cookies: &[&str], body: &Value) {
    let body = body.to_string();
    let mut head = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for c in cookies {
        head.push_str(&format!("Set-Cookie: {c}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
    stream.shutdown().await.ok();
}

async fn next(conn: &mut SyncConnection) -> SyncEvent {
    tokio::time::timeout(Duration::from_secs(10), conn.next_event())
        .await
        .expect("等事件超时")
        .expect("连接没了")
}

#[tokio::test]
async fn sync_protocol_against_a_fake_server() {
    let fake = Fake::start("right").await;
    let client = fake.client("right");
    assert_eq!(client.client_version().await.unwrap(), "0.27.5-fake");
    assert_eq!(client.user_id().await.unwrap(), "u1");

    let mut conn = client.sync().await.unwrap();
    assert!(conn.is_connected().await);
    conn.join("w", &["d1", "d2"]).await.unwrap();
    let joins = fake.events("space:join-batch");
    assert_eq!(joins.len(), 1);
    assert_eq!(joins[0].1["clientVersion"], "0.27.5-fake");
    assert_eq!(joins[0].1["protocolVersion"], 1);
    assert_eq!(
        joins[0].1["spaces"],
        json!([
            {"spaceType": "workspace", "spaceId": "w", "docId": "d1"},
            {"spaceType": "workspace", "spaceId": "w", "docId": "d2"},
        ])
    );
    // 加入过的不再加入
    conn.join("w", &["d1"]).await.unwrap();
    assert_eq!(fake.events("space:join-batch").len(), 1);

    // 服务端推来的修改
    match next(&mut conn).await {
        SyncEvent::DocUpdates {
            doc_id,
            updates,
            editor,
            timestamp,
            ..
        } => {
            assert_eq!(doc_id, "d1");
            assert_eq!(updates, vec![vec![4, 5]]);
            assert_eq!(editor.as_deref(), Some("someone"));
            assert_eq!(timestamp, 10);
        }
        other => panic!("{other:?}"),
    }

    let loaded = conn.load_doc("w", "d1", Some(&[9])).await.unwrap().unwrap();
    assert_eq!(
        (loaded.update, loaded.state_vector, loaded.timestamp),
        (vec![1, 2], vec![3], 7)
    );
    assert_eq!(
        fake.events("space:load-doc")[0].1["stateVector"],
        BASE64.encode([9u8])
    );
    assert!(conn.load_doc("w", "missing", None).await.unwrap().is_none());

    let pushed = conn
        .push_update("w", "d2", &[7, 7], Intent::Update)
        .await
        .unwrap();
    assert_eq!(pushed.timestamp, 9);
    let push = &fake.events("space:push-doc-update")[0].1;
    assert_eq!(
        (push["intent"].as_str(), push["update"].as_str()),
        (Some("update"), Some("Bwc="))
    );

    let ts = conn.doc_timestamps("w", Some(4)).await.unwrap();
    assert_eq!(ts.get("d2"), Some(&6));
    assert_eq!(
        fake.events("space:load-doc-timestamps")[0].1["timestamp"],
        4
    );

    // 断掉：后台重连，重新加入断之前加入了的文档
    fake.kill.notify_waiters();
    assert!(matches!(
        next(&mut conn).await,
        SyncEvent::Disconnected { .. }
    ));
    assert!(matches!(
        conn.load_doc("w", "d1", None).await,
        Err(Error::Disconnected) | Ok(_)
    ));
    loop {
        match next(&mut conn).await {
            SyncEvent::Reconnected => break,
            SyncEvent::DocUpdates { .. } => {}
            other => panic!("{other:?}"),
        }
    }
    let rejoin: Vec<Value> = fake
        .events("space:join-batch")
        .into_iter()
        .filter(|(c, _)| *c == 2)
        .flat_map(|(_, v)| v["spaces"].as_array().unwrap().clone())
        .collect();
    let mut docs: Vec<&str> = rejoin.iter().filter_map(|s| s["docId"].as_str()).collect();
    docs.sort();
    assert_eq!(docs, ["d1", "d2", "missing"]);
    assert!(conn.is_connected().await);

    let seen = fake.seen.lock().unwrap();
    assert!(seen.pongs > 0, "要回服务端的心跳");
    assert!(seen.cookies.iter().all(|c| c.contains("affine_session=s1")));
}

#[tokio::test]
async fn wrong_password_is_an_auth_error() {
    let fake = Fake::start("right").await;
    let err = fake.client("wrong").sync().await.err().unwrap();
    assert!(
        matches!(err, Error::Auth(ref m) if m.contains("WRONG_SIGN_IN_CREDENTIALS")),
        "{err}"
    );
    assert!(!err.is_transient());
}

#[tokio::test]
async fn unreachable_server_is_a_transient_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let client = Client::new(Config {
        url,
        credentials: Credentials {
            email: "a@b".into(),
            password: "p".into(),
        },
        client_version: Some("0.27.5".into()),
        extra_ca: None,
    })
    .unwrap();
    let err = client.sync().await.err().unwrap();
    assert!(err.is_transient(), "{err}");
}
