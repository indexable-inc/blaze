//! Embeds the site: `SITE_SNAPSHOT` (the snapshot gen/build.ts writes to .gen/site.cbor) and every file under `SITE_ASSETS` (gen/build.ts's dist/)
//! except its .html, which this component renders itself.

use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let p = e.expect("dir entry").path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_none_or(|x| x != "html") {
            out.push(p);
        }
    }
}

fn main() {
    println!("cargo::rerun-if-env-changed=SITE_SNAPSHOT");
    println!("cargo::rerun-if-env-changed=SITE_ASSETS");
    let snapshot = std::env::var("SITE_SNAPSHOT")
        .expect("SITE_SNAPSHOT: the .gen/site.cbor gen/build.ts wrote");
    let assets =
        PathBuf::from(std::env::var("SITE_ASSETS").expect("SITE_ASSETS: gen/build.ts's dist/"));
    let mut files = Vec::new();
    walk(&assets, &mut files);
    files.sort();
    let mut s = format!(
        "pub static SNAPSHOT: &[u8] = include_bytes!({snapshot:?});\npub static ASSETS: &[(&str, &[u8])] = &[\n"
    );
    for f in &files {
        let url = format!("/{}", f.strip_prefix(&assets).expect("under assets").to_string_lossy());
        s += &format!("    ({url:?}, include_bytes!({:?})),\n", f.canonicalize().expect("exists"));
        println!("cargo::rerun-if-changed={}", f.display());
    }
    s += "];\n";
    println!("cargo::rerun-if-changed={snapshot}");
    std::fs::write(
        PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("embedded.rs"),
        s,
    )
    .expect("write");
}
