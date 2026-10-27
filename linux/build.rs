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
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mac");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut files = Vec::new();
    walk(&root, &root, &mut files);
    let code = format!("pub const MAC_FILES: &[(&str, &[u8])] = &[\n{}\n];\n", files.join("\n"));
    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join("mac_files.rs"), code).unwrap();

    // The Mac app's installer, when one has been built (release-mac.sh → dist/Sweeflow.dmg):
    // carried in the binary, so `sweeflow install` can set it up for the "Add a Mac" window.
    let dmg = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dist/Sweeflow.dmg");
    println!("cargo:rerun-if-changed={}", dmg.display());
    let code = match fs::canonicalize(&dmg) {
        Ok(abs) => format!("pub const MAC_DMG: Option<&[u8]> = Some(include_bytes!({:?}));\n", abs.to_string_lossy()),
        Err(_) => "pub const MAC_DMG: Option<&[u8]> = None;\n".to_string(),
    };
    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join("mac_dmg.rs"), code).unwrap();
}
