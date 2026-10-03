//! Generate the gRPC code from `proto/ironweaver_db/v1` with protox (a
//! protobuf compiler in Rust), so building needs no `protoc`.

use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../proto");
    let dir = root.join("ironweaver_db/v1");
    let mut files: Vec<String> = std::fs::read_dir(&dir)?
        .filter_map(|entry| entry.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|name| name.ends_with(".proto"))
        .map(|name| format!("ironweaver_db/v1/{}", name))
        .collect();
    files.sort();
    let descriptors = protox::compile(&files, [&root])?;
    tonic_prost_build::configure()
        // Maps encode in key order, so equal messages encode to equal bytes
        .btree_map(".")
        .compile_fds(descriptors)?;
    println!("cargo:rerun-if-changed={}", root.display());
    Ok(())
}
