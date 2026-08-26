//! Wire-level client identity coverage for the subscription-backed Codex adapter.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use orca_harness_core::{Context, Model};
use orca_harness_model_providers::openai_codex::{
    BearerCredential, CodexCredential, CodexCredentialSource, CredentialError, CredentialSource,
    OpenAiCodexModel,
};

#[derive(Clone)]
struct Credentials;

#[async_trait]
impl CredentialSource for Credentials {
    async fn credential(&self) -> Result<BearerCredential, CredentialError> {
        Ok(bearer())
    }

    async fn refresh(&self) -> Result<BearerCredential, CredentialError> {
        Ok(bearer())
    }
}

#[async_trait]
impl CodexCredentialSource for Credentials {
    async fn codex_credential(&self) -> Result<CodexCredential, CredentialError> {
        Ok(CodexCredential {
            bearer: bearer(),
            account_id: "account-test".into(),
        })
    }

    async fn refresh_codex(
        &self,
        _rejected_access_token: &str,
    ) -> Result<CodexCredential, CredentialError> {
        self.codex_credential().await
    }
}

fn bearer() -> BearerCredential {
    BearerCredential {
        access_token: "token-test".into(),
        expires_at: None,
    }
}

async fn rejecting_server() -> (String, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let head = loop {
            let n = stream.read(&mut buf).await.unwrap();
            assert!(n > 0, "connection closed before request headers arrived");
            raw.extend_from_slice(&buf[..n]);
            if let Some(split) = raw.windows(4).position(|part| part == b"\r\n\r\n") {
                break String::from_utf8_lossy(&raw[..split]).to_lowercase();
            }
        };
        let _ = tx.send(head);
        stream
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
    });

    (format!("http://{addr}/codex"), rx)
}

#[tokio::test]
async fn generation_identifies_orcacode_without_impersonating_codex_cli() {
    let (base_url, captured) = rejecting_server().await;
    let model = OpenAiCodexModel::new("gpt-test", Arc::new(Credentials)).base_url(base_url);
    let mut context = Context::new();
    context.push_user("hello");

    let error = model.generate(&context, &[]).await.unwrap_err();
    assert!(error.to_string().contains("400"));

    let head = captured.await.unwrap();
    assert!(head.contains("user-agent: orcacode/"));
    assert!(head.contains("originator: orcacode"));
    assert!(!head.contains("codex_cli_rs"));
    assert!(head.contains("chatgpt-account-id: account-test"));
}
