//! Client for Gate's management API (`minekube.gate.v1.GateService`), spoken
//! as ConnectRPC unary calls with JSON bodies.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const SERVICE: &str = "minekube.gate.v1.GateService";

#[derive(Clone)]
pub struct GateClient {
    http: reqwest::Client,
    base: String,
}

#[derive(Debug)]
pub enum GateError {
    /// Gate answered with a Connect error, e.g. code `not_found`.
    Rpc { code: String, message: String },
    Transport(reqwest::Error),
}

impl GateError {
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Rpc { code, .. } => Some(code),
            Self::Transport(_) => None,
        }
    }
}

impl fmt::Display for GateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc { code, message } => write!(f, "gate: {code}: {message}"),
            Self::Transport(e) => write!(f, "gate: {e}"),
        }
    }
}

impl std::error::Error for GateError {}

#[derive(Deserialize)]
struct ConnectError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

impl GateClient {
    /// `base` is the API's HTTP origin, e.g. `http://127.0.0.1:8080`.
    pub fn new(base: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("building HTTP client");
        Self {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
        }
    }

    pub async fn register_server(&self, name: &str, address: &str) -> Result<(), GateError> {
        self.call::<Value>("RegisterServer", json!({ "name": name, "address": address }))
            .await
            .map(drop)
    }

    pub async fn unregister_server(&self, name: &str) -> Result<(), GateError> {
        self.call::<Value>("UnregisterServer", json!({ "name": name }))
            .await
            .map(drop)
    }

    /// Registers `name`, replacing a stale registration left by a previous run.
    pub async fn register_or_replace(&self, name: &str, address: &str) -> Result<(), GateError> {
        match self.register_server(name, address).await {
            Err(e) if e.code() == Some("already_exists") => {
                self.unregister_server(name).await?;
                self.register_server(name, address).await
            }
            other => other,
        }
    }

    /// Moves an online player (username or UUID) to a registered server.
    pub async fn connect_player(&self, player: &str, server: &str) -> Result<(), GateError> {
        self.call::<Value>("ConnectPlayer", json!({ "player": player, "server": server }))
            .await
            .map(drop)
    }

    async fn call<T: DeserializeOwned>(&self, method: &str, body: Value) -> Result<T, GateError> {
        let response = self
            .http
            .post(format!("{}/{SERVICE}/{method}", self.base))
            .header("Connect-Protocol-Version", "1")
            .json(&body)
            .send()
            .await
            .map_err(GateError::Transport)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(GateError::Transport)?;
        if status.is_success() {
            return serde_json::from_slice(&bytes).map_err(|e| GateError::Rpc {
                code: "internal".into(),
                message: format!("bad response body: {e}"),
            });
        }
        let error = serde_json::from_slice::<ConnectError>(&bytes).unwrap_or(ConnectError {
            code: String::new(),
            message: String::new(),
        });
        Err(GateError::Rpc {
            code: if error.code.is_empty() {
                format!("http_{}", status.as_u16())
            } else {
                error.code
            },
            message: error.message,
        })
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// Serves one canned HTTP response per entry and returns each raw request.
    async fn mock(responses: Vec<(u16, &'static str)>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut conn, _) = listener.accept().await.unwrap();
                let mut buf = vec![0; 4096];
                let mut len = 0;
                // Headers, then a Content-Length body.
                loop {
                    len += conn.read(&mut buf[len..]).await.unwrap();
                    let text = String::from_utf8_lossy(&buf[..len]);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let content_length = text
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length: ").map(|v| v.trim().parse::<usize>().unwrap()))
                            .unwrap_or(0);
                        if len >= end + 4 + content_length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8_lossy(&buf[..len]).into_owned());
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                conn.write_all(reply.as_bytes()).await.unwrap();
            }
            requests
        });
        (base, task)
    }

    #[tokio::test]
    async fn connect_player_posts_json() {
        let (base, server) = mock(vec![(200, "{}")]).await;
        GateClient::new(base).connect_player("Steve", "gh-1").await.unwrap();
        let requests = server.await.unwrap();
        let req = &requests[0];
        assert!(req.starts_with("POST /minekube.gate.v1.GateService/ConnectPlayer HTTP/1.1"), "{req}");
        assert!(req.to_ascii_lowercase().contains("content-type: application/json"));
        assert!(req.ends_with(r#"{"player":"Steve","server":"gh-1"}"#), "{req}");
    }

    #[tokio::test]
    async fn surfaces_connect_errors() {
        let (base, _server) = mock(vec![(404, r#"{"code":"not_found","message":"player not found"}"#)]).await;
        let err = GateClient::new(base).connect_player("ghost", "lobby").await.unwrap_err();
        assert_eq!(err.code(), Some("not_found"));
        assert_eq!(err.to_string(), "gate: not_found: player not found");
    }

    #[tokio::test]
    async fn replaces_stale_registration() {
        let (base, server) = mock(vec![
            (409, r#"{"code":"already_exists","message":"exists"}"#),
            (200, "{}"),
            (200, "{}"),
        ])
        .await;
        GateClient::new(base)
            .register_or_replace("gh-2", "127.0.0.1:25612")
            .await
            .unwrap();
        let methods: Vec<_> = server
            .await
            .unwrap()
            .iter()
            .map(|r| r.split_whitespace().nth(1).unwrap().rsplit('/').next().unwrap().to_owned())
            .collect();
        assert_eq!(methods, ["RegisterServer", "UnregisterServer", "RegisterServer"]);
    }
}
