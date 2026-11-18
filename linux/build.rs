//! Embeds the Mac host's sources (../mac) so `sweeflow setup` can install them over SSH.

use std::{env, fs, path::Path};

fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
    let mut entries: Vec<_> = fs::read_dir(dir).unwrap().flatten().collect();
    entries.sort_by_key(|e| e.path());
    for e in entries {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // .build, .swiftpm, dotfiles
        }
        if p.is_dir() {
            walk(&p, root, out);
        } else {
            // Each file: a directory only counts as changed when files come or go.
            println!("cargo:rerun-if-changed={}", p.display());
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            let abs = fs::canonicalize(&p).unwrap();
            out.push(format!("({rel:?}, include_bytes!({:?}) as &[u8]),", abs.to_string_lossy()));
        }
    }
}

fn main() {
    // FFmpeg 7.1 (libavfilter 10.4) and later take a picture's colour space and range in a
    // buffer source's parameters; older ones (Ubuntu 24.04 / Mint 22: FFmpeg 6.1) don't.
    println!("cargo::rustc-check-cfg=cfg(avfilter_colour)");
    let avfilter = std::process::Command::new("pkg-config").args(["--modversion", "libavfilter"]).output();
    if let Ok(o) = avfilter {
        let v: Vec<u32> = String::from_utf8_lossy(&o.stdout).trim().split('.').filter_map(|n| n.parse().ok()).collect();
        if v.len() >= 2 && (v[0], v[1]) >= (10, 4) {
            println!("cargo::rustc-cfg=avfilter_colour");
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mac");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut files = Vec::new();
    walk(&root, &root, &mut files);
    let code = format!("pub const MAC_FILES: &[(&str, &[u8])] = &[\n{}\n];\n", files.join("\n"));
    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join("mac_files.rs"), code).unwrap();
}
