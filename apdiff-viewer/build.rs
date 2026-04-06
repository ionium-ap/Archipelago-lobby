use sha2::{Digest, Sha256};
use walkdir::WalkDir;

fn main() {
    println!("cargo:rerun-if-changed=templates/");
    println!("cargo:rerun-if-changed=static/");

    let mut css_hasher = Sha256::new();
    for entry in WalkDir::new("static/css") {
        let entry = entry.unwrap();
        if entry.file_type().is_file() {
            css_hasher.update(std::fs::read(entry.path()).unwrap());
        }
    }
    let css_hash = css_hasher.finalize();
    println!("cargo:rustc-env=CSS_VERSION={css_hash:x}");
}
