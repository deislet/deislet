fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos: &[&str] = &["proto/telemetry.proto"];
    tonic_build::configure()
        // The client is generated for this crate's own integration tests and
        // for anyone who prefers depending on the crate over recompiling the
        // proto; the server is what the binary actually serves.
        .build_server(true)
        .build_client(true)
        .compile_protos(protos, &["proto"])?;
    for p in protos {
        println!("cargo:rerun-if-changed={p}");
    }
    Ok(())
}
