//! Embeds the web UI (FR-UI-01): every file in `ui/` is compiled into the
//! binary, with a gzip copy made at build time. Fails the build if the assets
//! exceed the 250 KB budget.

use flate2::{write::GzEncoder, Compression};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::PathBuf;

const BUDGET: usize = 250 * 1024;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ui = manifest.join("ui");
    println!("cargo:rerun-if-changed={}", ui.display());

    let mut entries: Vec<_> = std::fs::read_dir(&ui)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .collect();
    entries.sort();

    let mut total = 0;
    let mut code = String::from("pub static ASSETS: &[Asset] = &[\n");
    for path in entries {
        println!("cargo:rerun-if-changed={}", path.display());
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        let content_type = match path.extension().and_then(|e| e.to_str()) {
            Some("html") => "text/html; charset=utf-8",
            Some("js") => "text/javascript; charset=utf-8",
            Some("css") => "text/css; charset=utf-8",
            Some("svg") => "image/svg+xml",
            other => panic!("unexpected UI asset type {other:?} for {name}"),
        };
        let raw = std::fs::read(&path).unwrap();
        total += raw.len();
        let mut gz = GzEncoder::new(Vec::new(), Compression::best());
        gz.write_all(&raw).unwrap();
        let gz_path = out.join(format!("{name}.gz"));
        std::fs::write(&gz_path, gz.finish().unwrap()).unwrap();
        let mut h = DefaultHasher::new();
        raw.hash(&mut h);
        code.push_str(&format!(
            "    Asset {{ name: {name:?}, content_type: {content_type:?}, raw: include_bytes!({raw:?}), gzip: include_bytes!({gz:?}), etag: \"\\\"{etag:016x}\\\"\" }},\n",
            raw = path.display().to_string(),
            gz = gz_path.display().to_string(),
            etag = h.finish(),
        ));
    }
    code.push_str("];\n");
    assert!(
        total <= BUDGET,
        "UI assets are {total} bytes, over the {BUDGET} byte budget (FR-UI-01)"
    );
    std::fs::write(out.join("ui_assets.rs"), code).unwrap();
}
