use std::{
    env, fs,
    path::{Component, Path, PathBuf},
};

fn main() {
    // Embed the referenced assets: in the UKI boot environment there is no
    // source tree to load them from at runtime.
    #[cfg(feature = "uki")]
    slint_build::compile_with_config(
        "ui/boot_homepage.slint",
        slint_build::CompilerConfiguration::new()
            .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles),
    )
    .expect("failed to compile boot_homepage.slint");

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");

    // Generate rust_analyzer.json for the LSP. The analyzer indexes every
    // `config.rs` file across the workspace under its real crate-qualified
    // module path so that field-type lookups (e.g.
    // `sandpolis_instance::database::config::DatabaseConfig`) resolve
    // correctly at LSP serve time. Which of those types is the document root
    // depends on the file format being edited, so `sandpolis lsp` sets it from
    // its own flags rather than it being baked in here.
    //
    // Only `config.rs` files are read, so every type a config field names has
    // to be defined in one (re-export it from wherever else it's used).
    let workspace_root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("..");
    println!(
        "cargo:rerun-if-changed={}",
        workspace_root.join("Cargo.toml").display()
    );

    let mut analyzer = roniker::RustAnalyzer::new();

    for member_dir in workspace_member_dirs(&workspace_root) {
        let src_dir = member_dir.join("src");
        if !src_dir.is_dir() {
            continue;
        }

        let dir_name = member_dir
            .file_name()
            .and_then(|n| n.to_str())
            .expect("workspace member directory name must be valid UTF-8");

        // The binary `sandpolis` crate hosts the analyzer, so its types live
        // under `crate::…`. Every other workspace member is referenced by its
        // package name (directory name with `-` → `_`).
        let crate_prefix: String = if dir_name == "sandpolis" {
            "crate".into()
        } else {
            dir_name.replace('-', "_")
        };

        for entry in walkdir::WalkDir::new(&src_dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| e.file_name() == "config.rs")
        {
            println!("cargo:rerun-if-changed={}", entry.path().display());

            let rel = entry
                .path()
                .strip_prefix(&src_dir)
                .expect("walkdir entry lives under src_dir");

            let module_path: Vec<String> = rel
                .with_extension("")
                .components()
                .filter_map(|c| match c {
                    Component::Normal(os) => os.to_str().map(|s| s.to_owned()),
                    _ => None,
                })
                .collect();

            let prefix = if module_path.is_empty() {
                crate_prefix.clone()
            } else {
                format!("{}::{}", crate_prefix, module_path.join("::"))
            };

            let source = fs::read_to_string(entry.path()).expect("Failed to read config.rs source");

            // A field type spelled `crate::…` means this member's crate, but the
            // analyzer would read it as the `sandpolis` crate's. Name the crate
            // the way its types were indexed instead.
            let source = if crate_prefix == "crate" {
                source
            } else {
                source.replace("crate::", &format!("{crate_prefix}::"))
            };
            let _ = analyzer.add_source_with_prefix(&prefix, &source);
        }
    }

    let json = serde_json::to_string(&analyzer).expect("Failed to serialize RustAnalyzer");
    let dest = PathBuf::from(&out_dir).join("rust_analyzer.json");
    fs::write(&dest, json).expect("Failed to write rust_analyzer.json");
}

/// Direct subdirectories of the workspace root that contain a `Cargo.toml`.
fn workspace_member_dirs(workspace_root: &Path) -> Vec<PathBuf> {
    let mut members = Vec::new();
    let Ok(read_dir) = fs::read_dir(workspace_root) else {
        return members;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() && path.join("Cargo.toml").is_file() {
            members.push(path);
        }
    }
    members
}
