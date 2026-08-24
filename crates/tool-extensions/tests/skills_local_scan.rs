//! A measurement, not an assertion: how long discovery takes against
//! the machine it runs on, with whatever skills the developer actually
//! has installed. The benchmarks use a synthetic tree; this says what
//! the real one costs.
//!
//! `cargo test -p orca-harness-tool-extensions --test local_scan -- --ignored --nocapture`

use std::time::Instant;

#[test]
#[ignore = "local measurement: reads the developer's own skill folders"]
fn time_a_real_scan() {
    let workspace = std::env::current_dir().expect("cwd");
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let config = home.as_ref().map(|home| home.join(".config/orcacode"));
    let roots =
        orca_harness_tool_extensions::skills::roots(&workspace, config.as_deref(), home.as_deref());

    // One cold pass (the directory entries may not be cached), then a
    // warm average — the warm number is what a /skills toggle pays.
    let cold = Instant::now();
    let found = orca_harness_tool_extensions::skills::discover(&roots);
    let cold = cold.elapsed();

    let runs = 20;
    let warm = Instant::now();
    for _ in 0..runs {
        let _ = orca_harness_tool_extensions::skills::discover(&roots);
    }
    let warm = warm.elapsed() / runs;

    println!(
        "roots: {} · skills: {} · shadowed: {} · failed: {}",
        roots.len(),
        found.skills.len(),
        found.shadowed.len(),
        found.failures.len()
    );
    for root in &roots {
        let present = if root.path.is_dir() { "•" } else { " " };
        println!("  {present} {}", root.path.display());
    }
    println!("cold scan: {cold:?}");
    println!("warm scan: {warm:?} (mean of {runs})");
}
