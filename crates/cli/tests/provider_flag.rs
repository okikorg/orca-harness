//! `--provider`, `--list-providers` and `--list-models --json` through the
//! real binary, with a saved config pointing at a different provider and a
//! local fake endpoint. The environment is cleared so no real key leaks in.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

static NEXT: AtomicU64 = AtomicU64::new(1);
const SAVED_KEY: &str = "sk-or-test-secret-value";

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    /// Saved provider vercel, a saved openrouter key and model.
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "orcacode-provider-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::create_dir_all(root.join("ws")).unwrap();
        std::fs::write(
            root.join("config/config.json"),
            json!({
                "provider": "vercel",
                "api_keys": {"openrouter": SAVED_KEY},
                "models": {"openrouter": "saved/model", "local": "saved-local"},
            })
            .to_string(),
        )
        .unwrap();
        Self { root }
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.root.join("config/config.json")).unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_orcacode"))
            .args(args)
            .current_dir(self.root.join("ws"))
            .env_clear()
            .env("HOME", &self.root)
            .env("ORCA_CONFIG_DIR", self.root.join("config"))
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// One request the fake endpoint received: lowercased head and raw body.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

/// Serves `GET /v1/models` with a two-model catalog and answers any POST
/// with a one-line streamed reply. Every request is recorded so tests can
/// check the key and model that actually went over the wire.
fn fake_endpoint() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let record = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            let head = loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break None;
                }
                raw.extend_from_slice(&buf[..n]);
                if let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&raw[..split]).to_string();
                    let length: usize = head
                        .to_lowercase()
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    while raw.len() < split + 4 + length {
                        let n = stream.read(&mut buf).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        raw.extend_from_slice(&buf[..n]);
                    }
                    let body = String::from_utf8_lossy(&raw[split + 4..]).to_string();
                    record.lock().unwrap().push((head.to_lowercase(), body));
                    break Some(head);
                }
            };
            let Some(head) = head else { continue };
            let (kind, body) = if head.starts_with("GET") {
                (
                    "application/json",
                    json!({"data": [
                        {"id": "m-one", "name": "Model One", "context_length": 128000,
                         "reasoning": {"supported_efforts": ["low", "high"]}},
                        {"id": "m-two"},
                    ]})
                    .to_string(),
                )
            } else {
                (
                    "text/event-stream",
                    format!(
                        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        json!({"choices": [{"index": 0, "delta": {"content": "hi"}}]}),
                        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
                    ),
                )
            };
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (url, seen)
}

/// The last chat request: its authorization header (if any) and model.
fn last_post(seen: &Seen) -> (Option<String>, Value) {
    let seen = seen.lock().unwrap();
    let (head, body) = seen
        .iter()
        .rev()
        .find(|(head, _)| head.starts_with("post"))
        .expect("no chat request reached the endpoint");
    let auth = head
        .lines()
        .find_map(|line| line.strip_prefix("authorization:"))
        .map(|value| value.trim().to_string());
    (
        auth,
        serde_json::from_str::<Value>(body).unwrap()["model"].clone(),
    )
}

#[test]
fn list_providers_json_reports_auth_configuration_and_saved_models() {
    let fixture = Fixture::new();
    let before = fixture.config();
    let output = fixture.run(&["--list-providers", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(!text.contains(SAVED_KEY), "a key value was printed");
    let doc: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(doc["active"], "vercel");
    let by = |label: &str| {
        doc["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["label"] == label)
            .unwrap_or_else(|| panic!("{label} missing"))
            .clone()
    };
    assert_eq!(by("openrouter")["configured"], true);
    assert_eq!(by("openrouter")["model"], "saved/model");
    assert_eq!(by("vercel")["configured"], false);
    assert_eq!(by("vercel")["auth"], "api_key");
    assert_eq!(by("openai-codex")["auth"], "oauth");
    assert_eq!(by("openai-codex")["configured"], false);
    assert_eq!(by("local")["configured"], true);
    assert_eq!(by("local")["model"], "saved-local");
    assert_eq!(doc["providers"].as_array().unwrap().len(), 7);

    // --provider changes what is active for this invocation only.
    let output = fixture.run(&["--provider", "anthropic", "--list-providers", "--json"]);
    let doc: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(doc["active"], "anthropic");
    assert_eq!(fixture.config(), before, "config.json must not change");
}

#[test]
fn list_models_json_uses_the_requested_provider() {
    let fixture = Fixture::new();
    let (url, _) = fake_endpoint();
    let output = fixture.run(&[
        "--provider",
        "local",
        "--base-url",
        &url,
        "--list-models",
        "--json",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let doc: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(
        doc,
        json!({"provider": "local", "models": [
            {"id": "m-one", "name": "Model One", "contextWindow": 128000, "efforts": ["low", "high"]},
            {"id": "m-two", "name": null, "contextWindow": null, "efforts": null},
        ]})
    );
    // Plain text is unchanged without --json.
    let output = fixture.run(&["--provider", "local", "--base-url", &url, "--list-models"]);
    assert!(
        stdout(&output).starts_with("m-one  128k ctx\nm-two"),
        "{}",
        stdout(&output)
    );

    // A failing listing exits non-zero with the error on stderr.
    let output = fixture.run(&[
        "--provider",
        "local",
        "--base-url",
        "http://127.0.0.1:1/v1",
        "--list-models",
        "--json",
    ]);
    assert!(!output.status.success());
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("error"), "{}", stderr(&output));
}

#[test]
fn headless_runs_on_the_requested_provider_and_reports_its_label() {
    let fixture = Fixture::new();
    let before = fixture.config();
    let (url, seen) = fake_endpoint();
    for (provider, model) in [("local", "saved-local"), ("openrouter", "saved/model")] {
        let output = fixture.run(&[
            "--provider",
            provider,
            "--base-url",
            &url,
            "-p",
            "hello",
            "--json",
            "--no-session",
        ]);
        assert!(output.status.success(), "{provider}: {}", stderr(&output));
        let metadata: Value =
            serde_json::from_str(stdout(&output).lines().next().unwrap()).unwrap();
        assert_eq!(metadata["type"], "metadata");
        assert_eq!(metadata["provider"], provider);
        assert_eq!(metadata["model"], model, "saved model for {provider}");
        assert!(!stdout(&output).contains(SAVED_KEY));
        assert!(!stderr(&output).contains(SAVED_KEY));
        // What actually went over the wire: the chosen provider's model and
        // key. Local needs no key; openrouter sends its saved one.
        let (auth, sent_model) = last_post(&seen);
        assert_eq!(sent_model, model, "{provider} request model");
        let expected =
            (provider == "openrouter").then(|| format!("bearer {}", SAVED_KEY.to_lowercase()));
        assert_eq!(auth, expected, "{provider} authorization");
    }
    assert_eq!(fixture.config(), before, "config.json must not change");
}

#[test]
fn bad_provider_flags_exit_one_with_a_clear_message() {
    let fixture = Fixture::new();
    for (args, expected) in [
        (
            &["--provider", "nope", "--list-providers"][..],
            "unknown provider: nope",
        ),
        (
            &["--provider", "vercel", "--openrouter", "--list-models"][..],
            "cannot be combined with --openrouter",
        ),
        (
            &["--provider", "openai", "--anthropic", "--list-models"][..],
            "cannot be combined with --anthropic",
        ),
    ] {
        let output = fixture.run(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(
            stderr(&output).contains(expected),
            "{args:?}: {}",
            stderr(&output)
        );
    }
    // Agreeing flags are fine.
    let output = fixture.run(&[
        "--provider",
        "openrouter",
        "--openrouter",
        "--list-providers",
        "--json",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
}
