use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use walkdir::WalkDir;

fn main() -> Result<()> {
    let css_hash = compile_stylesheets()?;
    println!("cargo:rustc-env=CSS_VERSION={css_hash}");

    let mut js_hasher = Sha256::new();
    for entry in WalkDir::new("static/js") {
        let entry = entry.unwrap();
        if entry.file_type().is_file() {
            js_hasher.update(std::fs::read_to_string(entry.path())?);
        }
    }

    let js_hash = js_hasher.finalize();
    println!("cargo:rustc-env=JS_VERSION={js_hash:x}");

    println!("cargo:rustc-env=GIT_VERSION={}", derive_git_version()?);

    Ok(())
}

/// Compiles every stylesheet of `static/sass` to CSS. The CSS is not kept in the repository:
/// it goes to the build directory, along with `stylesheets.rs`, a table of it that the lobby
/// includes and serves under `/static/css/`. Returns a hash of all of it, for the URLs.
///
/// A file whose name starts with `_` is a partial, there to be imported by another one.
fn compile_stylesheets() -> Result<String> {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let css_dir = out_dir.join("css");
    std::fs::create_dir_all(&css_dir)?;

    let mut sources = Vec::new();
    for entry in std::fs::read_dir("static/sass")? {
        let path = entry?.path();
        let is_stylesheet = matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("sass" | "scss")
        );
        let is_partial = path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('_'));
        if is_stylesheet && !is_partial {
            sources.push(path);
        }
    }
    sources.sort();

    let options = grass::Options::default().style(grass::OutputStyle::Compressed);
    let mut hasher = Sha256::new();
    let mut table = String::from("pub const STYLESHEETS: &[(&str, &str)] = &[\n");
    for source in sources {
        let css = grass::from_path(&source, &options)
            .map_err(|e| anyhow!("{e}"))
            .with_context(|| format!("Compiling {}", source.display()))?;
        hasher.update(&css);

        let name = format!(
            "{}.css",
            source
                .file_stem()
                .context("A stylesheet without a name")?
                .to_string_lossy()
        );
        let css_path = css_dir.join(&name);
        std::fs::write(&css_path, css)?;
        table.push_str(&format!("    ({name:?}, include_str!({css_path:?})),\n"));
    }
    table.push_str("];\n");
    std::fs::write(out_dir.join("stylesheets.rs"), table)?;

    Ok(format!("{:x}", hasher.finalize()))
}

fn derive_git_version() -> Result<String> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?).join("../");
    let repo = git2::Repository::open(manifest_dir)?;
    let head = repo.head()?;
    let branch_name = head.name().unwrap().trim_start_matches("refs/heads/");

    let mut walk = repo.revwalk()?;
    walk.push(head.target().unwrap())?;
    let number = walk.count();

    Ok(format!("{branch_name}-{number}"))
}
