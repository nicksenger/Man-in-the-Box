//! WIT interface definitions for MITB policy plugins.

/// The main world.wit definition.
pub const WORLD_WIT: &str = include_str!("../wit/world.wit");
/// Tree-sitter component capability definitions.
pub const TREESITTER_WIT: &str = include_str!("../wit/deps/treesitter.wit");

/// WASI CLI dependency.
pub const WASI_CLI_WIT: &str = include_str!("../wit/deps/cli.wit");

/// WASI clocks dependency.
pub const WASI_CLOCKS_WIT: &str = include_str!("../wit/deps/clocks.wit");

/// WASI filesystem dependency.
pub const WASI_FILESYSTEM_WIT: &str = include_str!("../wit/deps/filesystem.wit");

/// WASI random dependency.
pub const WASI_RANDOM_WIT: &str = include_str!("../wit/deps/random.wit");

/// WASI sockets dependency.
pub const WASI_SOCKETS_WIT: &str = include_str!("../wit/deps/sockets.wit");

/// WASI HTTP dependency.
pub const WASI_HTTP_WIT: &str = include_str!("../wit/deps/http.wit");

/// Write all WIT files to the given directory, creating subdirectories as needed.
pub fn write_wit_to(dir: &std::path::Path) -> std::io::Result<()> {
    let deps = dir.join("deps");
    std::fs::create_dir_all(&deps)?;
    let legacy_treesitter = dir.join("treesitter.wit");
    if legacy_treesitter.exists() {
        std::fs::remove_file(legacy_treesitter)?;
    }
    std::fs::write(dir.join("world.wit"), WORLD_WIT)?;
    std::fs::write(deps.join("treesitter.wit"), TREESITTER_WIT)?;
    std::fs::write(deps.join("cli.wit"), WASI_CLI_WIT)?;
    std::fs::write(deps.join("clocks.wit"), WASI_CLOCKS_WIT)?;
    std::fs::write(deps.join("filesystem.wit"), WASI_FILESYSTEM_WIT)?;
    std::fs::write(deps.join("http.wit"), WASI_HTTP_WIT)?;
    std::fs::write(deps.join("random.wit"), WASI_RANDOM_WIT)?;
    std::fs::write(deps.join("sockets.wit"), WASI_SOCKETS_WIT)?;
    Ok(())
}
