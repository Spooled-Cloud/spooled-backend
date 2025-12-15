//! Build script for Spooled Backend
//!
//! Compiles protocol buffer definitions using tonic-build.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Compile the protobuf definitions
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        // Generate file descriptor set for reflection
        .file_descriptor_set_path(
            std::path::PathBuf::from(std::env::var("OUT_DIR")?).join("spooled_descriptor.bin"),
        )
        .compile_protos(&["proto/spooled.proto"], &["proto/"])?;

    // Re-run build if proto file changes
    println!("cargo:rerun-if-changed=proto/spooled.proto");

    Ok(())
}
