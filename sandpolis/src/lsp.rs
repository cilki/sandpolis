use anyhow::{Result, bail};
use roniker::RustAnalyzer;
use tracing::debug;

/// Which file format the LSP serves.
///
/// Realm configs are the only RON documents Sandpolis has, but the flag is
/// still required: it picks the root type the document's top level is resolved
/// against, and every completion and diagnostic follows from that.
#[derive(clap::Args, Debug, Clone)]
pub struct LspArgs {
    /// Serve realm configs (`*.realm.ron`)
    #[clap(long, required = true)]
    pub realm: bool,
}

impl LspArgs {
    /// Crate-qualified path of the type a document deserializes as, named the
    /// way `build.rs` indexed it.
    pub fn root_type(&self) -> &'static str {
        "crate::config::RealmConfig"
    }
}

pub async fn run(args: LspArgs) -> Result<()> {
    let mut rust_analyzer: RustAnalyzer = serde_json::from_str(include_str!(concat!(
        env!("OUT_DIR"),
        "/rust_analyzer.json"
    )))
    .expect("Failed to deserialize RustAnalyzer");

    // The analyzer indexes every config type in the workspace; which one is the
    // document root is decided here rather than at build time.
    let root_type = args.root_type();
    if !rust_analyzer.has_type(root_type) {
        bail!(
            "{root_type} is missing from the analyzer index, so no completions \
             could be served for it"
        );
    }
    rust_analyzer.root_type = Some(root_type.to_string());

    debug!(
        types = rust_analyzer.type_count(),
        root = root_type,
        "Starting LSP server"
    );

    roniker::serve(rust_analyzer, false).await;
    Ok(())
}

#[cfg(test)]
mod test_lsp_args {
    use super::*;

    /// The root type has to be in the index `build.rs` produced, or the LSP
    /// would come up serving nothing.
    #[test]
    fn the_root_type_is_indexed() {
        let analyzer: RustAnalyzer = serde_json::from_str(include_str!(concat!(
            env!("OUT_DIR"),
            "/rust_analyzer.json"
        )))
        .expect("the generated index deserializes");

        let root_type = LspArgs { realm: true }.root_type();
        assert!(
            analyzer.has_type(root_type),
            "{root_type} is missing from the generated index"
        );
    }

    /// A config field whose type the index lacks draws an `unknown-type` error
    /// on whatever value a realm config writes for it, correct or not — which
    /// is what happens when the type lives outside a `config.rs` file, or is
    /// named by a `crate::` path.
    #[test]
    fn every_config_field_type_is_indexed() {
        let analyzer: RustAnalyzer = serde_json::from_str(include_str!(concat!(
            env!("OUT_DIR"),
            "/rust_analyzer.json"
        )))
        .expect("the generated index deserializes");

        // Newtypes parsed from a string. Indexing their definitions wouldn't
        // help: the analyzer would expect the struct, not the string a realm
        // config writes for them.
        const PARSED_FROM_STRINGS: &[&str] = &["RealmName", "ServerUrl", "UserName", "Permission"];

        let unknown: Vec<_> = analyzer
            .validate_field_types()
            .into_iter()
            .filter(|(_, _, ty)| !PARSED_FROM_STRINGS.contains(&ty.as_str()))
            .filter(|(owner, field, _)| {
                // A skipped field never appears in a document.
                !analyzer
                    .get_type_info(owner)
                    .and_then(|info| info.find_field(field))
                    .is_some_and(|field| field.skip)
            })
            .collect();
        assert!(unknown.is_empty(), "config field types missing from the index: {unknown:#?}");
    }
}
