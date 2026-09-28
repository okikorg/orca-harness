//! Offline SSE framing timing. Ignored by default; run with
//! `cargo test --release -p orca-harness-model-providers sse_bench -- --ignored --nocapture`.
use crate::sse::SseBuffer;
use std::hint::black_box;
use std::time::Instant;

const EVENTS: usize = 10_000;
/// Network-sized reads, as `pump` receives them from `bytes_stream`.
const CHUNK: usize = 2 * 1024;

fn stats(label: &str, mut work: impl FnMut()) {
    for _ in 0..10 {
        work();
    }
    let mut samples: Vec<f64> = (0..100)
        .map(|_| {
            let start = Instant::now();
            work();
            start.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    samples.sort_by(f64::total_cmp);
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    println!("{label:<48} mean {mean:>9.3} ms  p95 {:>9.3} ms", samples[94]);
}

/// Responses API text deltas, about 150 bytes of JSON per event.
fn stream() -> Vec<u8> {
    let mut out = String::new();
    for i in 0..EVENTS {
        out.push_str("event: response.output_text.delta\n");
        out.push_str(&format!(
            "data: {{\"type\":\"response.output_text.delta\",\"item_id\":\"msg_0\",\"output_index\":0,\"content_index\":0,\"sequence_number\":{i},\"delta\":\"token \"}}\n\n"
        ));
    }
    out.into_bytes()
}

#[test]
#[ignore]
fn sse_10k_events() {
    let bytes = stream();
    println!("{} bytes, {CHUNK}-byte chunks", bytes.len());
    stats("SSE framing, 10k events", || {
        let mut buf = SseBuffer::default();
        let mut n = 0;
        for chunk in bytes.chunks(CHUNK) {
            n += black_box(buf.push(chunk).unwrap()).len();
        }
        assert_eq!(n, EVENTS);
    });
    stats("SSE framing + JSON decode, 10k events", || {
        let mut buf = SseBuffer::default();
        for chunk in bytes.chunks(CHUNK) {
            for payload in buf.push(chunk).unwrap() {
                black_box(serde_json::from_str::<serde_json::Value>(&payload).unwrap());
            }
        }
    });
}
