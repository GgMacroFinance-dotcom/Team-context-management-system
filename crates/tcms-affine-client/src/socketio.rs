//! socket.io 客户端（Engine.IO 4、Socket.IO 5），只做 AFFiNE 用到的那一点：
//! 只走 WebSocket，只用默认命名空间 `/`，只收发 JSON（AFFiNE 把 Yjs 更新编成 base64 字符串，用不到二进制附件）。
//!
//! 连上以后有一个后台任务管这条连接：回服务端的心跳、按编号把回执交给等着的调用、把服务端推来的事件转出去。
//! 很久没收到心跳、连接出错或者被关掉，后台任务就结束，等着的调用都拿到 [`Error::Disconnected`]。

use std::collections::HashMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use http::HeaderValue;
use http::header::HeaderName;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::Error;
use crate::net::{Endpoint, Tls, connect as connect_stream};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// AFFiNE 服务端默认一条消息最多 100 MB（`websocket.maxHttpBufferSize`），这边收的上限比它大一点。
const MAX_MESSAGE: usize = 128 << 20;

/// Engine.IO 的一个包。
#[derive(Debug, PartialEq)]
pub(crate) enum Frame {
    Open(Value),
    Close,
    Ping,
    Pong,
    Message(Packet),
    Upgrade,
    Noop,
}

/// Socket.IO 的一个包（Engine.IO 的 `4` 里面那层）。
#[derive(Debug, PartialEq)]
pub(crate) enum Packet {
    Connect(Value),
    Disconnect,
    Event {
        id: Option<u64>,
        args: Vec<Value>,
    },
    Ack {
        id: u64,
        args: Vec<Value>,
    },
    ConnectError(Value),
    /// 带二进制附件的事件或回执：AFFiNE 不用，收到就不管。
    Binary,
}

pub(crate) fn decode(text: &str) -> Result<Frame, Error> {
    let bad = || {
        Error::Protocol(format!(
            "看不懂的 socket.io 包：{}",
            crate::http::preview(text.as_bytes())
        ))
    };
    let mut chars = text.chars();
    let frame = match chars.next().ok_or_else(bad)? {
        '0' => Frame::Open(serde_json::from_str(chars.as_str()).map_err(|_| bad())?),
        '1' => Frame::Close,
        '2' => Frame::Ping,
        '3' => Frame::Pong,
        '4' => Frame::Message(decode_packet(chars.as_str()).ok_or_else(bad)?),
        '5' => Frame::Upgrade,
        '6' => Frame::Noop,
        _ => return Err(bad()),
    };
    Ok(frame)
}

fn decode_packet(s: &str) -> Option<Packet> {
    let kind = s.chars().next().filter(char::is_ascii_digit)?;
    let mut rest = &s[1..];
    if matches!(kind, '5' | '6') {
        return Some(Packet::Binary);
    }
    // 命名空间（不是 `/` 时才写）：`/名字,`
    if rest.starts_with('/') {
        rest = &rest[rest.find(',')? + 1..];
    }
    let digits = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let id = if digits > 0 {
        Some(rest[..digits].parse::<u64>().ok()?)
    } else {
        None
    };
    rest = &rest[digits..];
    let data: Option<Value> = if rest.is_empty() {
        None
    } else {
        Some(serde_json::from_str(rest).ok()?)
    };
    let args = |v: Option<Value>| match v {
        Some(Value::Array(a)) => Some(a),
        None => Some(Vec::new()),
        _ => None,
    };
    Some(match kind {
        '0' => Packet::Connect(data.unwrap_or(Value::Null)),
        '1' => Packet::Disconnect,
        '2' => Packet::Event {
            id,
            args: args(data)?,
        },
        '3' => Packet::Ack {
            id: id?,
            args: args(data)?,
        },
        '4' => Packet::ConnectError(data.unwrap_or(Value::Null)),
        _ => return None,
    })
}

pub(crate) fn encode_connect(auth: &Value) -> String {
    format!("40{auth}")
}

pub(crate) fn encode_event(id: Option<u64>, name: &str, data: &Value) -> String {
    let id = id.map(|i| i.to_string()).unwrap_or_default();
    format!("42{id}{}", json!([name, data]))
}

const PONG: &str = "3";
const DISCONNECT: &str = "41";

/// 服务端推来的东西。
#[derive(Debug)]
pub(crate) enum Incoming {
    Event {
        name: String,
        args: Vec<Value>,
    },
    /// 连接没了，后面不会再有事件。
    Closed(String),
}

enum Cmd {
    Emit {
        name: String,
        data: Value,
        ack: Option<oneshot::Sender<Vec<Value>>>,
    },
    Close,
}

/// 一条连上了的 socket.io 连接。丢掉它连接就关掉。
pub(crate) struct Socket {
    cmd: mpsc::UnboundedSender<Cmd>,
}

impl Socket {
    /// 发一个事件，等服务端的回执。
    pub(crate) async fn call(
        &self,
        name: &str,
        data: Value,
        timeout: Duration,
    ) -> Result<Vec<Value>, Error> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Emit {
                name: name.to_owned(),
                data,
                ack: Some(tx),
            })
            .map_err(|_| Error::Disconnected)?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(args)) => Ok(args),
            Ok(Err(_)) => Err(Error::Disconnected),
            Err(_) => Err(Error::Timeout(name.to_owned())),
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.cmd.is_closed()
    }

    pub(crate) fn close(&self) {
        let _ = self.cmd.send(Cmd::Close);
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.close();
    }
}

/// 连上 `/socket.io/`，做完 Engine.IO 和 Socket.IO 的握手。
pub(crate) async fn connect(
    endpoint: &Endpoint,
    tls: &Tls,
    headers: &[(HeaderName, String)],
    auth: &Value,
) -> Result<(Socket, mpsc::UnboundedReceiver<Incoming>), Error> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake(endpoint, tls, headers, auth))
        .await
        .map_err(|_| Error::Network(format!("和 {endpoint} 的 socket.io 握手超时")))?
}

type Ws = tokio_tungstenite::WebSocketStream<crate::net::Stream>;

async fn handshake(
    endpoint: &Endpoint,
    tls: &Tls,
    headers: &[(HeaderName, String)],
    auth: &Value,
) -> Result<(Socket, mpsc::UnboundedReceiver<Incoming>), Error> {
    let stream = connect_stream(endpoint, tls).await?;
    let url = endpoint.ws_url("/socket.io/?EIO=4&transport=websocket");
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| Error::Config(format!("WebSocket 地址不对：{e}")))?;
    for (name, value) in headers {
        let value = HeaderValue::from_str(value)
            .map_err(|_| Error::Config(format!("请求头 {name} 不对")))?;
        request.headers_mut().insert(name.clone(), value);
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let (mut ws, _) = tokio_tungstenite::client_async_with_config(request, stream, Some(config))
        .await
        .map_err(|e| match e {
            tokio_tungstenite::tungstenite::Error::Http(res) => Error::Network(format!(
                "{endpoint} 不让升级成 WebSocket：HTTP {}",
                res.status()
            )),
            e => Error::Network(format!("和 {endpoint} 的 WebSocket 握手失败：{e}")),
        })?;

    let open = match next_frame(&mut ws).await? {
        Frame::Open(v) => v,
        other => return Err(Error::Protocol(format!("第一个包应该是 open：{other:?}"))),
    };
    let ms = |key: &str, default: u64| open.get(key).and_then(Value::as_u64).unwrap_or(default);
    // 服务端隔 pingInterval 发一次心跳，过了 pingTimeout 还没等到回就断开；这边等心跳也用这两个数加起来。
    let silence = Duration::from_millis(ms("pingInterval", 25_000) + ms("pingTimeout", 20_000));

    send(&mut ws, encode_connect(auth)).await?;
    loop {
        match next_frame(&mut ws).await? {
            Frame::Ping => send(&mut ws, PONG.to_owned()).await?,
            Frame::Message(Packet::Connect(_)) => break,
            Frame::Message(Packet::ConnectError(v)) => {
                let message = v
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string());
                return Err(Error::Auth(message));
            }
            Frame::Close | Frame::Message(Packet::Disconnect) => {
                return Err(Error::Network("服务端在握手时关了连接".into()));
            }
            _ => {}
        }
    }

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    tokio::spawn(run(ws, cmd_rx, in_tx, silence));
    Ok((Socket { cmd: cmd_tx }, in_rx))
}

async fn next_frame(ws: &mut Ws) -> Result<Frame, Error> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return decode(t.as_str()),
            Some(Ok(Message::Close(_))) | None => return Err(Error::Network("连接关了".into())),
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(Error::Network(e.to_string())),
        }
    }
}

async fn send(ws: &mut Ws, text: String) -> Result<(), Error> {
    ws.send(Message::text(text))
        .await
        .map_err(|e| Error::Network(format!("发不出去：{e}")))
}

async fn run(
    mut ws: Ws,
    mut cmd: mpsc::UnboundedReceiver<Cmd>,
    incoming: mpsc::UnboundedSender<Incoming>,
    silence: Duration,
) {
    let mut pending: HashMap<u64, oneshot::Sender<Vec<Value>>> = HashMap::new();
    let mut next_id: u64 = 0;
    let mut deadline = Instant::now() + silence;
    let reason = loop {
        tokio::select! {
            msg = ws.next() => match msg {
                Some(Ok(Message::Text(t))) => {
                    deadline = Instant::now() + silence;
                    match decode(t.as_str()) {
                        Ok(Frame::Ping) => {
                            if let Err(e) = send(&mut ws, PONG.to_owned()).await {
                                break e.to_string();
                            }
                        }
                        Ok(Frame::Message(Packet::Event { args, .. })) => {
                            let mut args = args.into_iter();
                            if let Some(Value::String(name)) = args.next() {
                                let _ = incoming.send(Incoming::Event { name, args: args.collect() });
                            }
                        }
                        Ok(Frame::Message(Packet::Ack { id, args })) => {
                            if let Some(tx) = pending.remove(&id) {
                                let _ = tx.send(args);
                            }
                        }
                        Ok(Frame::Message(Packet::Disconnect)) => break "服务端断开了".to_owned(),
                        Ok(Frame::Close) => break "服务端关了连接".to_owned(),
                        Ok(_) => {}
                        Err(e) => break e.to_string(),
                    }
                }
                Some(Ok(Message::Close(_))) | None => break "连接关了".to_owned(),
                Some(Ok(_)) => {}
                Some(Err(e)) => break format!("连接出错：{e}"),
            },
            c = cmd.recv() => match c {
                Some(Cmd::Emit { name, data, ack }) => {
                    let id = ack.map(|tx| {
                        next_id += 1;
                        pending.insert(next_id, tx);
                        next_id
                    });
                    if let Err(e) = send(&mut ws, encode_event(id, &name, &data)).await {
                        break e.to_string();
                    }
                }
                Some(Cmd::Close) | None => {
                    let _ = send(&mut ws, DISCONNECT.to_owned()).await;
                    let _ = ws.close(None).await;
                    break "这边关了连接".to_owned();
                }
            },
            _ = tokio::time::sleep_until(deadline) => break format!("{} 秒没收到服务端的心跳", silence.as_secs()),
        }
    };
    drop(pending);
    let _ = incoming.send(Incoming::Closed(reason));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_are_decoded() {
        assert_eq!(
            decode(r#"0{"sid":"x","pingInterval":25000}"#).unwrap(),
            Frame::Open(json!({"sid":"x","pingInterval":25000}))
        );
        assert_eq!(decode("2").unwrap(), Frame::Ping);
        assert_eq!(
            decode(r#"40{"sid":"abc"}"#).unwrap(),
            Frame::Message(Packet::Connect(json!({"sid":"abc"})))
        );
        assert_eq!(
            decode("40").unwrap(),
            Frame::Message(Packet::Connect(Value::Null))
        );
        assert_eq!(
            decode(r#"44{"message":"You must sign in first"}"#).unwrap(),
            Frame::Message(Packet::ConnectError(
                json!({"message":"You must sign in first"})
            ))
        );
        assert_eq!(
            decode(r#"42["space:broadcast-doc-updates",{"docId":"d"}]"#).unwrap(),
            Frame::Message(Packet::Event {
                id: None,
                args: vec![json!("space:broadcast-doc-updates"), json!({"docId":"d"})]
            })
        );
        assert_eq!(
            decode(r#"4312[{"data":{"accepted":true}}]"#).unwrap(),
            Frame::Message(Packet::Ack {
                id: 12,
                args: vec![json!({"data":{"accepted":true}})]
            })
        );
        assert_eq!(
            decode(r#"42/admin,7["e",1]"#).unwrap(),
            Frame::Message(Packet::Event {
                id: Some(7),
                args: vec![json!("e"), json!(1)]
            })
        );
        assert_eq!(
            decode("451-[\"e\",{\"_placeholder\":true,\"num\":0}]").unwrap(),
            Frame::Message(Packet::Binary)
        );
        for bad in ["", "9", "4", "43[1]", "42{", "0nope", "42\"x\""] {
            assert!(decode(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn packets_are_encoded() {
        assert_eq!(encode_connect(&json!({})), "40{}");
        assert_eq!(
            encode_event(Some(3), "space:load-doc", &json!({"docId":"d"})),
            r#"423["space:load-doc",{"docId":"d"}]"#
        );
        assert_eq!(encode_event(None, "e", &json!(null)), r#"42["e",null]"#);
    }
}
