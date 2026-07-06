use std::path::Path;

/// tonic codegen needs `protoc`; the generated code is committed under
/// `src/api/` so plain `cargo install equalizer` works without protobuf
/// installed — codegen only reruns when protoc is actually available.
fn have_protoc() -> bool {
    if std::env::var_os("PROTOC").is_some() {
        return true;
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join("protoc").is_file()))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    if !have_protoc() && Path::new("src/api/equalizer.v1.rs").exists() {
        return Ok(());
    }
    tonic_build::configure()
        .out_dir("src/api")
        .file_descriptor_set_path("src/api/descriptor.bin")
        .compile_protos(&["proto/equalizer/v1/equalizer.proto"], &["proto"])?;
    Ok(())
}
