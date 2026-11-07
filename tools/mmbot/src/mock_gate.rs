//! Stands in for Gate's management API in handoff tests. It records server
//! registrations and reports `ConnectPlayer` calls, so a scenario can make
//! the switch itself by reconnecting the bot to the target backend.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct ConnectCall {
    pub player: String,
    pub server: String,
    pub address: Option<SocketAddr>,
}

pub struct MockGate {
    pub servers: Arc<Mutex<HashMap<String, SocketAddr>>>,
    pub connects: mpsc::UnboundedReceiver<ConnectCall>,
}

pub async fn start(bind: SocketAddr, lobby: SocketAddr) -> anyhow::Result<MockGate> {
    let listener = TcpListener::bind(bind).await?;
    let servers = Arc::new(Mutex::new(HashMap::from([("lobby".to_owned(), lobby)])));
    let (tx, rx) = mpsc::unbounded_channel();
    let state = servers.clone();
    tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            let (state, tx) = (state.clone(), tx.clone());
            tokio::spawn(async move {
                let _ = serve(conn, state, tx).await;
            });
        }
    });
    Ok(MockGate {
        servers,
        connects: rx,
    })
}

async fn serve(
    mut conn: TcpStream,
    servers: Arc<Mutex<HashMap<String, SocketAddr>>>,
    connects: mpsc::UnboundedSender<ConnectCall>,
) -> anyhow::Result<()> {
    // reqwest keeps connections alive, so serve requests until EOF.
    let mut buf = Vec::new();
    loop {
        let (method, body) = loop {
            if let Some(end) = find(&buf, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                let len = head
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                    .unwrap_or(0usize);
                if buf.len() >= end + 4 + len {
                    let body: Value = serde_json::from_slice(&buf[end + 4..end + 4 + len]).unwrap_or(Value::Null);
                    buf.drain(..end + 4 + len);
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
                    break (path.rsplit('/').next().unwrap_or("").to_owned(), body);
                }
            }
            let mut chunk = [0; 4096];
            let n = conn.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
        };

        let field = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        let (status, reply) = match method.as_str() {
            "RegisterServer" => {
                let addr = field("address").parse();
                match addr {
                    Ok(addr) => {
                        match servers.lock().unwrap().entry(field("name")) {
                            Entry::Occupied(_) => {
                                (409, r#"{"code":"already_exists","message":"server already registered"}"#)
                            }
                            Entry::Vacant(e) => {
                                e.insert(addr);
                                (200, "{}")
                            }
                        }
                    }
                    Err(_) => (400, r#"{"code":"invalid_argument","message":"bad address"}"#),
                }
            }
            "UnregisterServer" => {
                servers.lock().unwrap().remove(&field("name"));
                (200, "{}")
            }
            "ConnectPlayer" => {
                let server = field("server");
                let address = servers.lock().unwrap().get(&server).copied();
                let _ = connects.send(ConnectCall {
                    player: field("player"),
                    server,
                    address,
                });
                if address.is_some() {
                    (200, "{}")
                } else {
                    (404, r#"{"code":"not_found","message":"server not found"}"#)
                }
            }
            _ => (404, r#"{"code":"unimplemented","message":"unknown method"}"#),
        };
        let response = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{reply}",
            reply.len()
        );
        conn.write_all(response.as_bytes()).await?;
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
