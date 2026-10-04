//! Generate the gRPC code from `proto/ironweaver_db/v1` with protox (a
//! protobuf compiler in Rust), so building needs no `protoc`; the JSON
//! serde of the messages with pbjson (REST, ADR 0030); and keep the
//! descriptor set, from which the OpenAPI document is generated. The last
//! two only with the `rest` feature (ADR 0034).

use std::path::Path;

#[cfg(feature = "rest")]
use prost::Message;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `proto` in this crate is a symlink to the workspace's `proto/`, so that
    // a package of the crate (the Python sdist, ADR 0035) carries the protos
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("proto");
    let dir = root.join("ironweaver_db/v1");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files: Vec<String> = std::fs::read_dir(&dir)?
        .filter_map(|entry| entry.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|name| name.ends_with(".proto"))
        .map(|name| format!("ironweaver_db/v1/{}", name))
        .collect();
    files.sort();
    // With source info: the OpenAPI document takes its descriptions from
    // the protos' comments
    let descriptors = protox::compile(&files, [&root])?;
    #[cfg(feature = "rest")]
    let encoded = descriptors.encode_to_vec();
    tonic_prost_build::configure()
        // Maps encode in key order, so equal messages encode to equal bytes
        .btree_map(".")
        .compile_fds(descriptors)?;
    #[cfg(feature = "rest")]
    rest(&encoded)?;
    println!("cargo:rerun-if-changed={}", root.display());
    Ok(())
}

/// The descriptor set (for the OpenAPI document) and the messages' JSON serde.
#[cfg(feature = "rest")]
fn rest(encoded: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(out.join("descriptors.bin"), encoded)?;
    pbjson_build::Builder::new()
        .register_descriptors(encoded)?
        .btree_map(["."])
        // The core's types: their JSON form is the core's serde form, not
        // the wrapper messages' (src/rest/json.rs)
        .exclude([".ironweaver_db.v1.Value", ".ironweaver_db.v1.Expr", ".ironweaver_db.v1.Pattern"])
        .build(&[".ironweaver_db.v1"])?;
    Ok(())
}
