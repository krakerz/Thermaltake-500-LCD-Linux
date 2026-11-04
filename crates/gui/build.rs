use std::path::PathBuf;

/// Embeds data/icons/*.svg (see scripts/update-icons.sh) as a table the
/// app installs into its icon search path at startup.
fn embed_icons() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let dir = PathBuf::from(manifest).join("data/icons");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("data/icons is missing")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "svg"))
        .collect();
    entries.sort();
    let mut table = String::from("pub static ICONS: &[(&str, &[u8])] = &[\n");
    for path in &entries {
        let name = path
            .file_stem()
            .expect("an svg path has a file stem")
            .to_string_lossy();
        table.push_str(&format!(
            "    ({name:?}, include_bytes!({:?})),\n",
            path.display().to_string()
        ));
    }
    table.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("icons.rs");
    std::fs::write(out, table).expect("failed to write the icon table");
}

fn main() {
    embed_icons();
    println!("cargo:rerun-if-changed=build.rs");
}
