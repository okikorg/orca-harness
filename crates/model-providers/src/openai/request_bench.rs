use super::*;

#[test]
#[ignore = "offline release benchmark; run explicitly with --ignored --nocapture --test-threads=1"]
fn request_preparation_benchmark() {
    let start = std::time::Instant::now();
    drop(crate::http::client());
    println!(
        "shared_client_access_us={:.2}",
        start.elapsed().as_secs_f64() * 1e6
    );
    let model = OpenAiModel::new("test-model")
        .base_url("https://provider.invalid/v1")
        .api_key("offline-placeholder")
        .user_agent("offline-benchmark")
        .parallel_tool_calls(true);
    for (name, context, tools) in crate::request_bench::fixtures() {
        let body = model.request_body(&context, &tools);
        let request = model.prepare_request(&body).build().unwrap();
        let bytes = request.body().unwrap().as_bytes().unwrap().len();
        let encode = crate::request_bench::measure(|| model.request_body(&context, &tools));
        let prepare =
            crate::request_bench::measure(|| model.prepare_request(&body).build().unwrap());
        let total = crate::request_bench::measure(|| {
            model
                .prepare_request(&model.request_body(&context, &tools))
                .build()
                .unwrap()
        });
        println!("chat/{name}: bytes={bytes} encode_us={encode:.2} serialize_headers_build_us={prepare:.2} total_us={total:.2}");
    }
}

#[test]
fn prepared_request_retains_headers_url_and_exact_body_without_sending() {
    let model = OpenAiModel::new("test")
        .base_url("https://provider.invalid/v1/")
        .api_key("placeholder")
        .header("x-route", "route")
        .user_agent("client");
    let body = model.request_body(&Context::new(), &[]);
    let request = model.prepare_request(&body).build().unwrap();
    assert_eq!(
        request.url().as_str(),
        "https://provider.invalid/v1/chat/completions"
    );
    assert_eq!(request.method(), reqwest::Method::POST);
    assert_eq!(request.headers()["authorization"], "Bearer placeholder");
    assert_eq!(request.headers()["x-route"], "route");
    assert_eq!(request.headers()["user-agent"], "client");
    assert_eq!(request.headers()["content-type"], "application/json");
    assert_eq!(
        request.body().unwrap().as_bytes().unwrap(),
        serde_json::to_vec(&body).unwrap()
    );
}
