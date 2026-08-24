//! Web tools against a local HTTP server (policy opened for loopback).

use std::net::SocketAddr;
use std::sync::Arc;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use orca_harness_core::{CancellationToken, Tool, ToolContext};
use orca_harness_tool_extensions::web::{
    Firecrawl, UrlPolicy, WebCrawlTool, WebFetchTool, WebSearchTool,
};

fn ctx() -> ToolContext {
    ToolContext {
        call_id: "t".into(),
        tool_name: "t".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}

fn http_response(status: &str, content_type: &str, extra: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Serve canned responses, one connection each, in order, after `delay`.
async fn spawn_server_delayed(responses: Vec<String>, delay: std::time::Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for response in responses {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await; // request head; contents irrelevant
            tokio::time::sleep(delay).await;
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    addr
}

/// Serve canned responses, one connection each, in order.
async fn spawn_server(responses: Vec<String>) -> SocketAddr {
    spawn_server_delayed(responses, std::time::Duration::ZERO).await
}

#[tokio::test]
async fn fetch_converts_html_to_markdown() {
    let html = "<html><head><title>t</title></head><body>\
                <nav>Home Pricing</nav><h1>Title</h1><p>Hello <b>world</b></p>\
                <form><button>Subscribe</button></form>\
                <script>evil()</script></body></html>";
    let addr = spawn_server(vec![http_response(
        "200 OK",
        "text/html; charset=utf-8",
        "",
        html,
    )])
    .await;

    let tool = WebFetchTool::new(UrlPolicy::strict().allow_private());
    let out = tool
        .call(json!({"url": format!("http://{addr}/page")}), &ctx())
        .await
        .unwrap();
    let content = out["content"].as_str().unwrap();
    assert!(
        content.contains("# Title"),
        "markdown heading in: {content}"
    );
    assert!(content.contains("**world**"), "bold kept: {content}");
    assert!(!content.contains("evil"), "script stripped: {content}");
    assert!(
        !content.contains("Pricing"),
        "navigation stripped: {content}"
    );
    assert!(!content.contains("Subscribe"), "forms stripped: {content}");
    assert!(!content.contains('<'), "HTML tags stripped: {content}");
    assert_eq!(out["status"], json!(200));
    assert_eq!(out["truncated"], json!(false));
}

#[tokio::test]
async fn fetch_converts_html_with_a_wrong_content_type() {
    let html = "<!doctype html><html><body><main><h1>Useful</h1><p>Context</p></main>\
                <script>metadata()</script></body></html>";
    let addr = spawn_server(vec![http_response("200 OK", "text/plain", "", html)]).await;

    let tool = WebFetchTool::new(UrlPolicy::strict().allow_private());
    let out = tool
        .call(json!({"url": format!("http://{addr}/mislabelled")}), &ctx())
        .await
        .unwrap();
    let content = out["content"].as_str().unwrap();
    assert!(
        content.contains("# Useful"),
        "markdown heading in: {content}"
    );
    assert!(content.contains("Context"), "page content kept: {content}");
    assert!(!content.contains("metadata"), "script stripped: {content}");
    assert!(!content.contains('<'), "HTML tags stripped: {content}");
}

#[test]
fn fetch_schema_does_not_offer_raw_html() {
    let schema = WebFetchTool::default().schema();
    assert!(schema.parameters["properties"].get("raw").is_none());
}

#[tokio::test]
async fn fetch_follows_redirects_revetting_each_hop() {
    let addr_final = spawn_server(vec![http_response("200 OK", "text/plain", "", "arrived")]).await;
    let addr_first = spawn_server(vec![http_response(
        "302 Found",
        "text/plain",
        &format!("Location: http://{addr_final}/next\r\n"),
        "",
    )])
    .await;

    let tool = WebFetchTool::new(UrlPolicy::strict().allow_private());
    let out = tool
        .call(json!({"url": format!("http://{addr_first}/start")}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["content"], json!("arrived"));
    assert_eq!(out["status"], json!(200));
    assert!(out["finalUrl"].as_str().unwrap().ends_with("/next"));
}

#[tokio::test]
async fn fetch_refuses_binary_content() {
    let addr = spawn_server(vec![http_response(
        "200 OK",
        "application/octet-stream",
        "",
        "\u{0}\u{1}\u{2}",
    )])
    .await;
    let tool = WebFetchTool::new(UrlPolicy::strict().allow_private());
    let err = tool
        .call(json!({"url": format!("http://{addr}/blob")}), &ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unsupported content type"));
}

#[tokio::test]
async fn strict_policy_blocks_loopback_fetches() {
    let tool = WebFetchTool::default();
    let err = tool
        .call(json!({"url": "http://127.0.0.1:59999/"}), &ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("non-public"), "{err}");
}

#[tokio::test]
async fn search_parses_provider_results() {
    let body = json!({
        "success": true,
        "data": { "web": [
            {"title": "Result One", "url": "https://one.example", "description": "first hit", "position": 1},
            {"title": "Result Two", "url": "https://two.example", "description": "second hit", "position": 2},
        ]}
    })
    .to_string();
    let addr = spawn_server(vec![http_response("200 OK", "application/json", "", &body)]).await;

    let provider = Firecrawl::new("test-key").base_url(format!("http://{addr}"));
    let tool = WebSearchTool::new(Arc::new(provider));
    let out = tool
        .call(json!({"query": "orca harness", "count": 2}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["provider"], json!("firecrawl"));
    let results = out["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["title"], json!("Result One"));
    assert_eq!(results[1]["snippet"], json!("second hit"));
}

fn crawl_page(url: &str, title: &str, markdown: &str) -> serde_json::Value {
    json!({
        "markdown": markdown,
        "metadata": { "title": title, "sourceURL": url, "statusCode": 200 }
    })
}

#[tokio::test]
async fn crawl_polls_the_job_until_completed() {
    let start = json!({"success": true, "id": "job-1", "url": "http://site.example"}).to_string();
    let scraping =
        json!({"status": "scraping", "total": 2, "completed": 1, "data": []}).to_string();
    let completed = json!({
        "status": "completed", "total": 2, "completed": 2,
        "data": [
            crawl_page("http://site.example/", "Home", "# Home\n\nwelcome"),
            crawl_page("http://site.example/docs", "Docs", "# Docs\n\ndetails"),
        ]
    })
    .to_string();
    let addr = spawn_server(vec![
        http_response("200 OK", "application/json", "", &start),
        http_response("200 OK", "application/json", "", &scraping),
        http_response("200 OK", "application/json", "", &completed),
    ])
    .await;

    let fc = Arc::new(Firecrawl::new("test-key").base_url(format!("http://{addr}")));
    let tool = WebCrawlTool::new(fc).poll_interval(std::time::Duration::from_millis(20));
    let out = tool
        .call(json!({"url": "http://site.example", "limit": 2}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["status"], json!("completed"));
    assert_eq!(out["timedOut"], json!(false));
    assert_eq!(out["pagesReturned"], json!(2));
    let pages = out["pages"].as_array().unwrap();
    assert_eq!(pages[0]["title"], json!("Home"));
    assert!(pages[1]["markdown"].as_str().unwrap().contains("# Docs"));
    assert_eq!(pages[1]["truncated"], json!(false));
}

#[tokio::test]
async fn crawl_returns_partial_pages_when_the_wait_budget_runs_out() {
    let start = json!({"success": true, "id": "job-2"}).to_string();
    let scraping = json!({
        "status": "scraping", "total": 9, "completed": 1,
        "data": [crawl_page("http://slow.example/", "First", "only page so far")]
    })
    .to_string();
    let addr = spawn_server(vec![
        http_response("200 OK", "application/json", "", &start),
        http_response("200 OK", "application/json", "", &scraping),
    ])
    .await;

    let fc = Arc::new(Firecrawl::new("test-key").base_url(format!("http://{addr}")));
    let tool = WebCrawlTool::new(fc).max_wait(std::time::Duration::ZERO);
    let out = tool
        .call(json!({"url": "http://slow.example"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out["status"], json!("scraping"));
    assert_eq!(out["timedOut"], json!(true));
    assert_eq!(out["pagesReturned"], json!(1));
    assert_eq!(out["totalPages"], json!(9));
    assert_eq!(out["pages"][0]["title"], json!("First"));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_fetches_fan_out_through_the_dispatcher() {
    use orca_harness_core::testing::call;
    use orca_harness_core::{Dispatcher, ExtensionRegistry, ToolRegistry};
    use std::time::{Duration, Instant};

    // Three servers, each delaying 300ms before answering: a serial client
    // needs 900ms+, a parallel batch finishes near 300ms.
    let mut urls = Vec::new();
    for i in 0..3 {
        let addr = spawn_server_delayed(
            vec![http_response(
                "200 OK",
                "text/plain",
                "",
                &format!("body-{i}"),
            )],
            Duration::from_millis(300),
        )
        .await;
        urls.push(format!("http://{addr}/p{i}"));
    }

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(WebFetchTool::new(
        UrlPolicy::strict().allow_private(),
    )));
    let batch: Vec<_> = urls
        .iter()
        .enumerate()
        .map(|(i, url)| call(&format!("c{i}"), "web_fetch", json!({"url": url})))
        .collect();

    let started = Instant::now();
    let results = Dispatcher::new()
        .execute(
            batch,
            &tools,
            &ExtensionRegistry::new(),
            &orca_harness_core::CancellationToken::new(),
            None,
            3,
        )
        .await
        .unwrap();
    let wall = started.elapsed();

    for (i, result) in results.iter().enumerate() {
        assert!(!result.is_error, "fetch {i} failed: {}", result.output);
        assert_eq!(result.output["content"], json!(format!("body-{i}")));
    }
    assert!(
        wall < Duration::from_millis(700),
        "3×300ms fetches should overlap, took {wall:?}"
    );
}
