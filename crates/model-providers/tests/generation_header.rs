//! The generation id from OpenRouter's `X-Generation-Id` response header:
//! reported once per call as soon as the headers arrive, before any chunk,
//! even when the caller drops the call before the first chunk comes.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use orca_harness_core::{Context, Model};
use orca_harness_model_providers::openai::{OpenAiModel, ResponseIdHook, ResponseIdSource};

type Ids = Arc<Mutex<Vec<(String, ResponseIdSource)>>>;

fn recorded_ids() -> (Ids, ResponseIdHook) {
    let ids: Ids = Arc::new(Mutex::new(Vec::new()));
    let sink = ids.clone();
    let hook: ResponseIdHook = Arc::new(move |id: &str, source| {
        sink.lock().unwrap().push((id.to_owned(), source));
    });
    (ids, hook)
}

fn header(id: &str) -> Vec<(String, ResponseIdSource)> {
    vec![(id.to_owned(), ResponseIdSource::Header)]
}

fn body(id: &str) -> Vec<(String, ResponseIdSource)> {
    vec![(id.to_owned(), ResponseIdSource::Body)]
}

fn context() -> Context {
    let mut context = Context::new();
    context.push_user("hi");
    context
}

/// Reads one request off `stream`, through its body.
async fn read_request(stream: &mut tokio::net::TcpStream) {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf).await.unwrap();
        assert!(n > 0, "connection closed before full request arrived");
        raw.extend_from_slice(&buf[..n]);
        let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&raw[..split]).to_lowercase();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .map(|v| v.trim().parse().unwrap())
            .unwrap_or(0);
        if raw.len() >= split + 4 + length {
            return;
        }
    }
}

/// What the fixture saw: when it sent the headers, and when the client
/// closed the connection.
struct Seen {
    headers_sent: oneshot::Receiver<Instant>,
    closed: oneshot::Receiver<Instant>,
}

/// OpenRouter before its first token: the headers (with `X-Generation-Id`
/// when given), then `: OPENROUTER PROCESSING` keep-alives and no chunk,
/// until the client closes the connection. The headers wait `delay`.
async fn keep_alive_server(generation: Option<&'static str>, delay: Duration) -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (sent_tx, headers_sent) = oneshot::channel();
    let (closed_tx, closed) = oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        tokio::time::sleep(delay).await;
        let header = generation
            .map(|id| format!("x-generation-id: {id}\r\n"))
            .unwrap_or_default();
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n{header}connection: close\r\n\r\n"
        );
        if stream.write_all(head.as_bytes()).await.is_err() {
            let _ = closed_tx.send(Instant::now());
            return;
        }
        let _ = sent_tx.send(Instant::now());
        let (mut reader, mut writer) = stream.into_split();
        let keep_alive = async move {
            loop {
                if writer
                    .write_all(b": OPENROUTER PROCESSING\n\n")
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        let read_closed = async move {
            let mut buf = [0u8; 64];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
            }
        };
        tokio::select! {
            _ = keep_alive => {}
            _ = read_closed => {}
        }
        let _ = closed_tx.send(Instant::now());
    });
    (
        format!("http://{addr}/v1"),
        Seen {
            headers_sent,
            closed,
        },
    )
}

/// Waits up to `limit` for the hook to have been called.
async fn ids_within(ids: &Ids, limit: Duration) -> Vec<(String, ResponseIdSource)> {
    let until = Instant::now() + limit;
    while Instant::now() < until && ids.lock().unwrap().is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    ids.lock().unwrap().clone()
}

#[tokio::test]
async fn a_stream_dropped_after_its_headers_and_before_its_first_chunk_still_reports_its_id() {
    let (base_url, seen) = keep_alive_server(Some("gen-header"), Duration::ZERO).await;
    let (ids, hook) = recorded_ids();
    let model = OpenAiModel::new("test-model")
        .base_url(base_url)
        .response_id(hook);
    let context = context();

    // The caller drops the call once the fixture has sent its headers.
    tokio::select! {
        result = model.generate_streaming(&context, &[], &|_| {}) => {
            panic!("the stream ended before the drop: {result:?}")
        }
        sent = seen.headers_sent => { sent.unwrap(); }
    }
    let dropped = Instant::now();

    assert_eq!(
        ids_within(&ids, Duration::from_secs(2)).await,
        header("gen-header")
    );
    let closed = tokio::time::timeout(Duration::from_secs(2), seen.closed)
        .await
        .expect("the connection was closed")
        .unwrap();
    assert!(
        closed.saturating_duration_since(dropped) < Duration::from_secs(1),
        "closed {:?} after the drop",
        closed.saturating_duration_since(dropped)
    );
}

/// Sends the headers (with `X-Generation-Id`), then holds the body until
/// the hook has fired or `hold` passed, and sends whether it fired first.
async fn header_then_body_server(
    status: &'static str,
    content_type: &'static str,
    body: String,
    ids: Ids,
    hold: Duration,
) -> (String, oneshot::Receiver<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, fired) = oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        let head = format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\nx-generation-id: gen-header\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        let until = Instant::now() + hold;
        while Instant::now() < until && ids.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let _ = tx.send(!ids.lock().unwrap().is_empty());
        let _ = stream.write_all(body.as_bytes()).await;
        let _ = stream.shutdown().await;
    });
    (format!("http://{addr}/v1"), fired)
}

#[tokio::test]
async fn a_streamed_call_reports_the_header_id_before_any_chunk() {
    let (ids, hook) = recorded_ids();
    let stream = [
        r#"data: {"id":"gen-chunk","choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
        "data: [DONE]",
    ]
    .join("\n\n")
        + "\n\n";
    let (base_url, fired) = header_then_body_server(
        "200 OK",
        "text/event-stream",
        stream,
        ids.clone(),
        Duration::from_secs(2),
    )
    .await;
    let model = OpenAiModel::new("test-model")
        .base_url(base_url)
        .response_id(hook);

    model
        .generate_streaming(&context(), &[], &|_| {})
        .await
        .unwrap();

    assert!(fired.await.unwrap(), "the id came only with the body");
    assert_eq!(*ids.lock().unwrap(), header("gen-header"));
}

#[tokio::test]
async fn a_plain_call_reports_the_header_id_before_its_body() {
    let (ids, hook) = recorded_ids();
    let completion = serde_json::json!({
        "id": "gen-body",
        "choices": [{"finish_reason": "stop", "message": {"content": "ok"}}],
        "usage": {"prompt_tokens": 12, "completion_tokens": 7, "cost": 1.08e-05}
    })
    .to_string();
    let (base_url, fired) = header_then_body_server(
        "200 OK",
        "application/json",
        completion,
        ids.clone(),
        Duration::from_secs(2),
    )
    .await;
    let model = OpenAiModel::new("test-model")
        .base_url(base_url)
        .response_id(hook);

    model.generate(&context(), &[]).await.unwrap();

    assert!(fired.await.unwrap(), "the id came only with the body");
    assert_eq!(*ids.lock().unwrap(), header("gen-header"));
}

#[tokio::test]
async fn an_error_status_reports_no_id_even_with_the_header() {
    for streaming in [false, true] {
        for status in ["429 Too Many Requests", "500 Internal Server Error"] {
            let (ids, hook) = recorded_ids();
            let (base_url, fired) = header_then_body_server(
                status,
                "application/json",
                r#"{"error":{"code":429,"message":"slow down"}}"#.into(),
                ids.clone(),
                Duration::from_millis(300),
            )
            .await;
            let model = OpenAiModel::new("test-model")
                .base_url(base_url)
                .response_id(hook);
            let result = if streaming {
                model.generate_streaming(&context(), &[], &|_| {}).await
            } else {
                model.generate(&context(), &[]).await
            };

            assert!(result.is_err());
            assert!(!fired.await.unwrap());
            assert!(ids.lock().unwrap().is_empty(), "{status}");
        }
    }
}

#[tokio::test]
async fn a_call_dropped_before_its_headers_reports_the_id_once_they_come() {
    let (base_url, seen) =
        keep_alive_server(Some("gen-late-header"), Duration::from_millis(500)).await;
    let (ids, hook) = recorded_ids();
    let model = OpenAiModel::new("test-model")
        .base_url(base_url)
        .response_id(hook);
    let context = context();

    // Dropped 100 ms after sending, 400 ms before the headers come.
    let _ = tokio::time::timeout(
        Duration::from_millis(100),
        model.generate_streaming(&context, &[], &|_| {}),
    )
    .await
    .expect_err("the call ended before the drop");
    assert!(ids.lock().unwrap().is_empty());

    assert_eq!(
        ids_within(&ids, Duration::from_secs(3)).await,
        header("gen-late-header")
    );
    let sent = seen.headers_sent.await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(2), seen.closed)
        .await
        .expect("the connection was closed")
        .unwrap();
    assert!(closed.saturating_duration_since(sent) < Duration::from_secs(1));
}

#[tokio::test]
async fn a_call_dropped_before_its_headers_reports_nothing_after_the_wait() {
    let (base_url, seen) =
        keep_alive_server(Some("gen-too-late"), Duration::from_millis(800)).await;
    let (ids, hook) = recorded_ids();
    let model = OpenAiModel::new("test-model")
        .base_url(base_url)
        .response_id(hook)
        .header_wait(Duration::from_millis(200));
    let context = context();

    let _ = tokio::time::timeout(
        Duration::from_millis(100),
        model.generate_streaming(&context, &[], &|_| {}),
    )
    .await
    .expect_err("the call ended before the drop");

    // The wait ends 300 ms after sending; the headers would come at 800 ms.
    tokio::time::timeout(Duration::from_secs(3), seen.closed)
        .await
        .expect("the connection was closed")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(ids.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_default_wait_after_a_drop_is_ten_seconds() {
    assert_eq!(
        orca_harness_model_providers::openai::HEADER_WAIT,
        Duration::from_secs(10)
    );
}

#[tokio::test]
async fn with_no_header_the_first_chunks_id_is_reported_once() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        let body = [
            r#"data: {"id":"gen-chunk","choices":[{"delta":{"content":"o"}}]}"#,
            r#"data: {"id":"gen-chunk","choices":[{"delta":{"content":"k"},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ]
        .join("\n\n")
            + "\n\n";
        let reply = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(reply.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
    });
    let (ids, hook) = recorded_ids();
    let model = OpenAiModel::new("test-model")
        .base_url(format!("http://{addr}/v1"))
        .response_id(hook);

    model
        .generate_streaming(&context(), &[], &|_| {})
        .await
        .unwrap();

    assert_eq!(*ids.lock().unwrap(), body("gen-chunk"));
}
