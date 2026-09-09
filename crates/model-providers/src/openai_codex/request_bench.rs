use super::*;

fn credential() -> CodexCredential {
    CodexCredential {
        bearer: BearerCredential {
            access_token: "offline-placeholder".into(),
            expires_at: None,
        },
        account_id: "offline-account".into(),
    }
}

fn model() -> OpenAiCodexModel {
    // These tests use prepared credentials; this source is never queried.
    OpenAiCodexModel::new(
        "test-model",
        Arc::new(super::continuation_tests::UnusedCredentials),
    )
    .base_url("https://provider.invalid/codex/")
    .prompt_cache_key("stable")
}

#[test]
#[ignore = "offline release benchmark; run explicitly with --ignored --nocapture --test-threads=1"]
fn request_preparation_benchmark() {
    let model = model();
    for (name, context, tools) in crate::request_bench::fixtures() {
        let body = || {
            request::body(
                &model.model,
                &context,
                &tools,
                true,
                &[],
                None,
                Some("stable"),
            )
        };
        let encoded = body();
        let request = model
            .prepare_request(&encoded, true, credential())
            .build()
            .unwrap();
        let bytes = request.body().unwrap().as_bytes().unwrap().len();
        let encode = crate::request_bench::measure(body);
        let prepare = crate::request_bench::measure(|| {
            model
                .prepare_request(&encoded, true, credential())
                .build()
                .unwrap()
        });
        let total = crate::request_bench::measure(|| {
            model
                .prepare_request(&body(), true, credential())
                .build()
                .unwrap()
        });
        println!("responses/{name}: bytes={bytes} encode_us={encode:.2} serialize_headers_build_us={prepare:.2} total_us={total:.2}");
    }
}

#[test]
fn prepared_request_retains_codex_headers_and_body_without_sending() {
    let model = model();
    let body = request::body(
        "test-model",
        &Context::new(),
        &[],
        true,
        &[],
        None,
        Some("stable"),
    );
    for stream in [false, true] {
        let request = model
            .prepare_request(&body, stream, credential())
            .build()
            .unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://provider.invalid/codex/responses"
        );
        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(
            request.headers()["authorization"],
            "Bearer offline-placeholder"
        );
        assert_eq!(request.headers()["chatgpt-account-id"], "offline-account");
        assert_eq!(request.headers()["openai-beta"], "responses=experimental");
        assert_eq!(request.headers()["originator"], "orcacode");
        assert_eq!(
            request.headers()["accept"],
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            }
        );
        assert_eq!(request.headers()["user-agent"], ORCACODE_USER_AGENT);
        assert_eq!(
            request.body().unwrap().as_bytes().unwrap(),
            serde_json::to_vec(&body).unwrap()
        );
    }
}
