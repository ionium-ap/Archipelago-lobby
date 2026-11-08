//! Lists the JSON files of `builtins/` for `builtin.rs` to include, so that a filter becomes
//! builtin by putting its file there.

use std::fmt::Write;
use std::path::Path;

fn main() {
    let dir = Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("builtins");
    println!("cargo:rerun-if-changed={}", dir.display());

    let mut files = std::fs::read_dir(&dir)
        .expect("review-filters/builtins should exist")
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect::<Vec<_>>();
    files.sort();

    let mut out = String::from("&[\n");
    for path in files {
        println!("cargo:rerun-if-changed={}", path.display());
        let id = path.file_stem().unwrap().to_str().expect("UTF-8 file name");
        writeln!(
            out,
            "    ({:?}, include_str!({:?})),",
            id,
            path.to_str().expect("UTF-8 path")
        )
        .unwrap();
    }
    out.push(']');

    let out_path = Path::new(&std::env::var("OUT_DIR").unwrap()).join("builtin_filters.rs");
    std::fs::write(out_path, out).unwrap();
}
