use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../mitb-pake/Cargo.toml");
    println!("cargo:rerun-if-changed=../mitb-pake/src");

    build_pake_wasm()?;
    Ok(())
}

fn build_pake_wasm() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let workspace_dir = manifest_dir
        .parent()
        .ok_or_else(|| io::Error::other("mitb-server must live inside the workspace root"))?;
    let cargo = env::var("CARGO")?;
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let target_dir = workspace_dir.join("target/mitb-pake-wasm");

    let status = Command::new(cargo)
        .current_dir(workspace_dir)
        .env("CARGO_TARGET_DIR", &target_dir)
        .args([
            "build",
            "--package",
            "mitb-pake",
            "--target",
            "wasm32-unknown-unknown",
            "--release",
        ])
        .status()?;

    if !status.success() {
        return Err(io::Error::other("failed to build mitb-pake wasm module").into());
    }

    let wasm_path = target_dir.join("wasm32-unknown-unknown/release/mitb_pake.wasm");
    let output = out_dir.join("mitb-pake.wasm");
    copy_file(&wasm_path, &output)?;
    Ok(())
}

fn copy_file(from: &Path, to: &Path) -> Result<(), io::Error> {
    fs::copy(from, to).map(|_| ()).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "failed to copy wasm asset from {} to {}: {error}",
                from.display(),
                to.display()
            ),
        )
    })
}
