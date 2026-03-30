use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let wit_dir = out_dir.join("mitb-wit");

    mitb_wit::write_wit_to(&wit_dir)?;

    let bindgen_source = format!(
        r#"wasmtime::component::bindgen!({{
    path: "{}",
    world: "policy",
    imports: {{
        default: async | trappable,
    }},
    exports: {{
        default: async | store | task_exit,
    }},
    require_store_data_send: true,
    with: {{
        "mitb:host/process.child": crate::ProcessChild,
        "wasi": wasmtime_wasi::p3::bindings,
        "wasi:http": wasmtime_wasi_http::p3::bindings::http,
    }},
}});"#,
        wit_dir.display()
    );

    std::fs::write(out_dir.join("mitb_bindgen.rs"), bindgen_source)?;

    println!("cargo:rerun-if-changed=../mitb-wit/wit/world.wit");
    println!("cargo:rerun-if-changed=../mitb-wit/wit/deps/treesitter.wit");
    println!("cargo:rerun-if-changed=../mitb-wit/wit/deps/cli.wit");
    println!("cargo:rerun-if-changed=../mitb-wit/wit/deps/clocks.wit");
    println!("cargo:rerun-if-changed=../mitb-wit/wit/deps/filesystem.wit");
    println!("cargo:rerun-if-changed=../mitb-wit/wit/deps/http.wit");
    println!("cargo:rerun-if-changed=../mitb-wit/wit/deps/random.wit");
    println!("cargo:rerun-if-changed=../mitb-wit/wit/deps/sockets.wit");

    Ok(())
}
