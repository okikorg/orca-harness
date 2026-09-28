//! Offline timing for the `@` location picker. Ignored by default; run with
//! `cargo test --release -p <cli crate> search_bench -- --ignored --nocapture`.
use super::components::picker::ListPicker;
use super::composer::workspace_locations;
use super::state::{LocationEntry, LocationPicker};
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

/// Three warm queries: short and common, a directory prefix, one filename.
const QUERIES: [&str; 3] = ["mod", "pkg_42/", "module_99999.rs"];

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

fn rel(i: usize) -> String {
    format!("crates/pkg_{}/src/module_{i}.rs", i / 8)
}

fn picker(entries: Vec<LocationEntry>) -> LocationPicker {
    LocationPicker {
        picker: ListPicker::new(entries.len()),
        entries,
        query: String::new(),
        token_start: 0,
    }
}

fn hits(picker: &mut LocationPicker) -> Vec<usize> {
    QUERIES
        .iter()
        .map(|query| {
            picker.query = (*query).into();
            picker.filtered().len()
        })
        .collect()
}

fn run_queries(picker: &mut LocationPicker) {
    for query in QUERIES {
        picker.query = query.into();
        black_box(picker.filtered());
    }
}

/// Matcher only: every path indexed, no cap, as a persistent index would be.
#[test]
#[ignore]
fn filter_uncapped() {
    for files in [100_000usize, 500_000] {
        let entries = (0..files)
            .map(|i| LocationEntry {
                path: rel(i),
                directory: false,
            })
            .collect();
        let mut picker = picker(entries);
        println!("uncapped {files}: hits per query {:?}", hits(&mut picker));
        stats(&format!("filter uncapped {files} paths, 3 queries"), || {
            run_queries(&mut picker)
        });
    }
}

/// Product path: the walk paid on every `@` press, then the three queries
/// over whatever the 5,000-entry cap kept.
#[test]
#[ignore]
fn walk_and_filter() {
    for files in [100_000usize, 500_000] {
        let root = std::env::temp_dir().join(format!("orca-search-bench-{files}"));
        let _ = std::fs::remove_dir_all(&root);
        for i in 0..files {
            let path: PathBuf = root.join(rel(i));
            if i % 8 == 0 {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            }
            std::fs::write(path, b"").unwrap();
        }
        let entries = workspace_locations(&root);
        let kept_files = entries.iter().filter(|entry| !entry.directory).count();
        println!("{files} files on disk: {} entries kept, {kept_files} are files", entries.len());
        stats(&format!("walk {files} files (on @ press)"), || {
            black_box(workspace_locations(&root));
        });
        let mut picker = picker(entries);
        println!("capped {files}: hits per query {:?}", hits(&mut picker));
        stats(&format!("filter capped {files}, 3 queries"), || {
            run_queries(&mut picker)
        });
        std::fs::remove_dir_all(&root).unwrap();
    }
}
