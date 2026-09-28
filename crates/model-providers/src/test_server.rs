//! A local HTTP server for wire tests: one connection per scripted reply,
//! returning each captured request. Unit tests use `crate::test_server`;
//! integration tests include this file with `#[path]`.
#![allow(dead_code)]

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// One received request.
#[derive(Debug, Clone)]
pub struct Captured {
    /// Request line and headers, as sent.
    pub head: String,
    pub body: Vec<u8>,
}

impl Captured {
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("JSON request body")
    }
    /// The head lowercased, for case-insensitive header checks.
    pub fn lower(&self) -> String {
        self.head.to_ascii_lowercase()
    }
}

/// One scripted response.
pub struct Reply {
    status: String,
    headers: String,
    body: Vec<u8>,
}

impl Reply {
    pub fn new(status: &str, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: status.into(),
            headers: format!("Content-Type: {content_type}\r\n"),
            body: body.into(),
        }
    }
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push_str(&format!("{name}: {value}\r\n"));
        self
    }
}

pub fn json(body: impl Into<Vec<u8>>) -> Reply {
    Reply::new("200 OK", "application/json", body)
}

pub fn sse(body: impl Into<Vec<u8>>) -> Reply {
    Reply::new("200 OK", "text/event-stream", body)
}

pub fn status(status: &str, body: impl Into<Vec<u8>>) -> Reply {
    Reply::new(status, "application/json", body)
}

/// Serve `replies` in order, one per connection, and return every request.
pub async fn serve(replies: Vec<Reply>) -> (String, JoinHandle<Vec<Captured>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut captured = Vec::new();
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            captured.push(read_request(&mut socket).await);
            let head = format!(
                "HTTP/1.1 {}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
                reply.status,
                reply.headers,
                reply.body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&reply.body).await.unwrap();
        }
        captured
    });
    (url, task)
}

/// Read one request head and its `Content-Length` body.
pub async fn read_request(socket: &mut tokio::net::TcpStream) -> Captured {
    let mut bytes = Vec::new();
    let mut buf = [0; 8192];
    let split = loop {
        if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        let n = socket.read(&mut buf).await.unwrap();
        if n == 0 {
            break bytes.len();
        }
        bytes.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&bytes[..split]).into_owned();
    let length = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = bytes[split..].to_vec();
    while body.len() < length {
        let n = socket.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    Captured { head, body }
}
