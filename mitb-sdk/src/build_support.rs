use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BuildSupportError {
    #[error("failed to read OUT_DIR: {source}")]
    ReadOutDir {
        #[source]
        source: std::env::VarError,
    },
    #[error("failed to materialize mitb-wit in `{path}`: {source}")]
    WriteWit {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write generated bindgen source to `{path}`: {source}")]
    WriteBindgen {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Write guest-side WIT bindings for the MITB `policy` world into `OUT_DIR`.
pub fn write_policy_bindgen() -> Result<(), BuildSupportError> {
    write_bindgen_for_world_named("policy", "mitb_guest_bindgen.rs")
}

/// Same as [`write_policy_bindgen`] with a custom output file name.
pub fn write_policy_bindgen_named(output_file: &str) -> Result<(), BuildSupportError> {
    write_bindgen_for_world_named("policy", output_file)
}

/// Write guest-side WIT bindings for the MITB `treesitter-policy` world.
pub fn write_policy_bindgen_with_treesitter() -> Result<(), BuildSupportError> {
    write_bindgen_for_world_named("treesitter-policy", "mitb_guest_bindgen.rs")
}

/// Write guest-side WIT bindings for an arbitrary world into `OUT_DIR`.
pub fn write_bindgen_for_world_named(
    world: &str,
    output_file: &str,
) -> Result<(), BuildSupportError> {
    let out_dir = PathBuf::from(
        std::env::var("OUT_DIR").map_err(|source| BuildSupportError::ReadOutDir { source })?,
    );
    let wit_dir = out_dir.join("mitb-wit");
    mitb_wit::write_wit_to(&wit_dir).map_err(|source| BuildSupportError::WriteWit {
        path: wit_dir.clone(),
        source,
    })?;

    let bindgen = format!(
        r#"wit_bindgen::generate!({{
    path: "{}",
    world: "{}",
    generate_all,
}});"#,
        wit_dir.display(),
        world
    );

    let bindgen_path = out_dir.join(output_file);
    std::fs::write(&bindgen_path, bindgen).map_err(|source| BuildSupportError::WriteBindgen {
        path: bindgen_path,
        source,
    })?;
    Ok(())
}

/// Print cargo rerun directives for all MITB WIT files from this repository.
pub fn print_rerun_if_mitb_wit_changed() {
    print_rerun_if_mitb_wit_changed_at(&mitb_wit_source_root());
}

/// Print cargo rerun directives for all WIT files under `wit_root`.
pub fn print_rerun_if_mitb_wit_changed_at(wit_root: &Path) {
    println!(
        "cargo:rerun-if-changed={}",
        wit_root.join("world.wit").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        wit_root.join("deps/treesitter.wit").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        wit_root.join("deps/cli.wit").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        wit_root.join("deps/clocks.wit").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        wit_root.join("deps/filesystem.wit").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        wit_root.join("deps/http.wit").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        wit_root.join("deps/random.wit").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        wit_root.join("deps/sockets.wit").display()
    );
}

fn mitb_wit_source_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../mitb-wit/wit")
}
