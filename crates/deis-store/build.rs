fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos: &[&str] = &["proto/storage.proto", "proto/blob.proto"];
    tonic_build::configure()
        // The client stubs are not used by the binary; they exist so the
        // crate's own tests can drive the real services over a real socket
        // instead of calling the handlers directly.
        .build_server(true)
        .build_client(true)
        .compile_protos(protos, &["proto"])?;
    for p in protos {
        println!("cargo:rerun-if-changed={p}");
    }
    Ok(())
}
