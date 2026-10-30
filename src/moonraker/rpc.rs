//! JSON-RPC 2.0 over Moonraker's WebSocket.
//!
//! The hub's session task is the only user of a socket, so calls are sequential: send a
//! request, then read frames until its response arrives. Notifications that arrive meanwhile
//! are handed to a callback so no update is lost.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as Frame;

use super::MoonrakerError;
use super::client::MoonrakerSocket;

const CALL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum Message {
    Response {
        id: u64,
        result: Result<Value, MoonrakerError>,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// Pings, pongs and anything unrecognised.
    Other,
}

pub struct Rpc {
    ws: MoonrakerSocket,
    next_id: u64,
}

impl Rpc {
    pub fn new(ws: MoonrakerSocket) -> Self {
        Self { ws, next_id: 1 }
    }

    /// Reads one frame. Cancel-safe, so it can sit in a `select!`.
    pub async fn recv(&mut self) -> Result<Message, MoonrakerError> {
        match self.ws.next().await {
            Some(Ok(Frame::Text(text))) => Ok(parse(text.as_str())),
            Some(Ok(Frame::Close(_))) | None => Err(MoonrakerError::Offline("connection closed".into())),
            Some(Ok(_)) => Ok(Message::Other),
            Some(Err(e)) => Err(MoonrakerError::Offline(e.to_string())),
        }
    }

    pub async fn call(
        &mut self,
        method: &str,
        params: Value,
        on_notify: &mut impl FnMut(&str, &Value),
    ) -> Result<Value, MoonrakerError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc": "2.0", "method": method, "params": params, "id": id});
        self.ws
            .send(Frame::Text(request.to_string().into()))
            .await
            .map_err(|e| MoonrakerError::Offline(e.to_string()))?;
        tokio::time::timeout(CALL_TIMEOUT, async {
            loop {
                match self.recv().await? {
                    Message::Response { id: got, result } if got == id => return result,
                    Message::Notification { method, params } => on_notify(&method, &params),
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| MoonrakerError::Offline(format!("{method} timed out")))?
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}

fn parse(text: &str) -> Message {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return Message::Other;
    };
    if let Some(id) = v.get("id").and_then(Value::as_u64) {
        let result = match v.get("error") {
            Some(err) => Err(rpc_error(err)),
            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
        };
        return Message::Response { id, result };
    }
    match v.get("method").and_then(Value::as_str) {
        Some(method) => Message::Notification {
            method: method.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
        },
        None => Message::Other,
    }
}

fn rpc_error(err: &Value) -> MoonrakerError {
    let code = err.get("code").and_then(Value::as_i64).unwrap_or(500);
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("error")
        .to_string();
    match code {
        401 | 403 => MoonrakerError::Unauthorized,
        503 => MoonrakerError::KlippyNotReady(message),
        c => MoonrakerError::Http {
            status: u16::try_from(c)
                .ok()
                .filter(|s| (100..600).contains(s))
                .unwrap_or(500),
            message,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_responses_errors_and_notifications() {
        match parse(r#"{"jsonrpc":"2.0","result":{"a":1},"id":7}"#) {
            Message::Response { id: 7, result: Ok(v) } => assert_eq!(v["a"], 1),
            other => panic!("{other:?}"),
        }
        match parse(r#"{"jsonrpc":"2.0","error":{"code":503,"message":"Klippy Disconnected"},"id":2}"#) {
            Message::Response {
                id: 2,
                result: Err(MoonrakerError::KlippyNotReady(m)),
            } => assert_eq!(m, "Klippy Disconnected"),
            other => panic!("{other:?}"),
        }
        match parse(r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"Method not found"},"id":3}"#) {
            Message::Response {
                result: Err(MoonrakerError::Http { status: 500, .. }),
                ..
            } => {}
            other => panic!("{other:?}"),
        }
        match parse(r#"{"jsonrpc":"2.0","method":"notify_status_update","params":[{"fan":{"speed":1}},1.0]}"#)
        {
            Message::Notification { method, params } => {
                assert_eq!(method, "notify_status_update");
                assert_eq!(params[0]["fan"]["speed"], 1);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse("not json"), Message::Other));
    }
}
