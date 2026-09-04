use std::{env, fs, path::PathBuf};

fn main() {
    let releases = PathBuf::from("../../docs/releases");
    println!("cargo:rerun-if-changed={}", releases.display());
    let mut notes = fs::read_dir(&releases)
        .expect("read release notes")
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let version = path.file_stem()?.to_str()?.to_owned();
            let parts = version
                .split('.')
                .map(str::parse::<u64>)
                .collect::<Result<Vec<_>, _>>()
                .ok()?;
            (parts.len() == 3).then_some((parts, version, path))
        })
        .collect::<Vec<_>>();
    notes.sort_by(|a, b| b.0.cmp(&a.0));

    let entries = notes
        .into_iter()
        .map(|(_, version, path)| {
            format!(
                "({version:?}, include_str!({:?}))",
                path.canonicalize().unwrap()
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("release_notes.rs");
    fs::write(output, format!("&[{entries}]")).expect("write release notes module");
}
