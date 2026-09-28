//! Offline `grep` timing. Ignored by default; run with
//! `cargo test --release -p orca-harness-tools --test grep_timing -- --ignored --nocapture`.
use orca_harness_core::{CancellationToken, Tool, ToolContext};
use orca_harness_tools::{GrepTool, Workspace};
use serde_json::json;
use std::hint::black_box;
use std::time::Instant;

const FILES: usize = 1_000;
const HITS: usize = 200;
const LINES: usize = 80;
const LINE: &str =
    "    let outcome = registry.dispatch(call, &context).await.map_err(Error::from)?;";

#[test]
#[ignore]
fn grep_1000_files_200_matches() {
    let root = std::env::temp_dir().join(format!("orca-grep-timing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let body = format!("{LINE}\n").repeat(LINES);
    for i in 0..FILES {
        let dir = root.join(format!("crates/pkg_{}/src", i / 8));
        std::fs::create_dir_all(&dir).unwrap();
        let mut text = format!("//! module {i}\n{body}");
        if i % (FILES / HITS) == 0 {
            text.push_str("// NEEDLE-MARKER\n");
        }
        std::fs::write(dir.join(format!("module_{i}.rs")), text).unwrap();
    }
    // Raise the 200-result cap so the walk reads every file instead of
    // stopping at the 200th hit.
    let tool = GrepTool::new(Workspace::new(&root)).max_results(FILES);
    let input = json!({ "query": "NEEDLE-MARKER", "path": "." });
    let ctx = ToolContext {
        call_id: "grep-0".into(),
        tool_name: "grep".into(),
        cancellation: CancellationToken::new(),
        deadline: None,
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut run = || rt.block_on(tool.call(input.clone(), &ctx)).unwrap();
    assert_eq!(run()["matches"].as_array().unwrap().len(), HITS);
    for _ in 0..10 {
        black_box(run());
    }
    let mut samples: Vec<f64> = (0..100)
        .map(|_| {
            let start = Instant::now();
            black_box(run());
            start.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    samples.sort_by(f64::total_cmp);
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    println!(
        "grep {FILES} files x {} lines, {HITS} matches     mean {mean:>9.3} ms  p95 {:>9.3} ms",
        LINES + 1,
        samples[94]
    );
    std::fs::remove_dir_all(&root).unwrap();
}
