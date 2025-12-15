//! Build script for Spooled Backend
//!
//! Compiles protocol buffer definitions using tonic-prost-build.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let descriptor_path = out_dir.join("spooled_descriptor.bin");

    // Compile the protobuf definitions
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        // Generate file descriptor set for reflection
        .file_descriptor_set_path(&descriptor_path)
        .compile_protos(&["proto/spooled.proto"], &["proto/"])?;

    // Re-run build if proto file changes
    println!("cargo:rerun-if-changed=proto/spooled.proto");

    Ok(())
}
