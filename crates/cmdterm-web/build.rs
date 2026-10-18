//! Embeds the files `web/FILES.txt` lists, so the binary serves the web app
//! on its own.

use std::fmt::Write;
use std::path::Path;

fn main() {
    let web = Path::new(env!("CARGO_MANIFEST_DIR")).join("web");
    let list = web.join("FILES.txt");
    println!("cargo:rerun-if-changed={}", list.display());
    let text = std::fs::read_to_string(&list).unwrap_or_default();
    let mut out = String::from("&[\n");
    for path in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let file = web.join(path);
        assert!(
            file.is_file(),
            "web/FILES.txt names {path}, which does not exist"
        );
        println!("cargo:rerun-if-changed={}", file.display());
        writeln!(
            out,
            "    ({path:?}, include_bytes!({:?})),",
            file.display().to_string()
        )
        .unwrap();
    }
    out.push(']');
    let target = Path::new(&std::env::var("OUT_DIR").unwrap()).join("assets.rs");
    std::fs::write(target, out).unwrap();
}
