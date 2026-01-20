// build.rs
// Compiling Protocol Buffers definitions

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Compiling storage proto (client)
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["proto/storage.proto"], &["proto"])?;

    // Compiling logging proto (client - denix pushes logs)
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["proto/logging.proto"], &["proto"])?;

    // Compiling management proto (server - receives supervisor control)
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(&["proto/management.proto"], &["proto"])?;

    // Recompile when proto files change
    println!("cargo:rerun-if-changed=proto/storage.proto");
    println!("cargo:rerun-if-changed=proto/logging.proto");
    println!("cargo:rerun-if-changed=proto/management.proto");

    Ok(())
}
