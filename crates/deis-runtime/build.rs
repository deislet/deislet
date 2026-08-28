// build.rs
// Compiling Protocol Buffers definitions

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Compiling storage proto (client)
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["proto/storage.proto"], &["proto"])?;

    // Compiling queue proto (client)
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["proto/queue.proto"], &["proto"])?;

    // Compiling blob proto (client)
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["proto/blob.proto"], &["proto"])?;

    // Compiling logging proto (client - deis-runtime pushes logs)
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["proto/logging.proto"], &["proto"])?;

    // Compiling management proto (server - receives control control)
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(&["proto/management.proto"], &["proto"])?;

    // Compiling trigger proto (server)
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(&["proto/trigger.proto"], &["proto"])?;

    // Recompile when proto files change
    println!("cargo:rerun-if-changed=proto/storage.proto");
    println!("cargo:rerun-if-changed=proto/queue.proto");
    println!("cargo:rerun-if-changed=proto/blob.proto");
    println!("cargo:rerun-if-changed=proto/logging.proto");
    println!("cargo:rerun-if-changed=proto/management.proto");
    println!("cargo:rerun-if-changed=proto/trigger.proto");

    Ok(())
}
