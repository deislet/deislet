// build.rs
// Compiling Protocol Buffers definitions

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Compiling storage proto (client)
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["../deis-store/proto/storage.proto"], &["../deis-store/proto"])?;

    // Compiling queue proto (client)
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["../deis-queue/proto/queue.proto"], &["../deis-queue/proto"])?;

    // Compiling blob proto (client)
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["../deis-store/proto/blob.proto"], &["../deis-store/proto"])?;

    // Compiling logging proto (client - deis-runtime pushes logs)
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["../deis-telemetry/proto/logging.proto"], &["../deis-telemetry/proto"])?;

    // Compiling management proto (server - receives control-plane commands)
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
    println!("cargo:rerun-if-changed=../deis-store/proto/storage.proto");
    println!("cargo:rerun-if-changed=../deis-queue/proto/queue.proto");
    println!("cargo:rerun-if-changed=../deis-store/proto/blob.proto");
    println!("cargo:rerun-if-changed=../deis-telemetry/proto/logging.proto");
    println!("cargo:rerun-if-changed=proto/management.proto");
    println!("cargo:rerun-if-changed=proto/trigger.proto");

    Ok(())
}
