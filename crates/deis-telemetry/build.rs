fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos: &[&str] = &["proto/logging.proto"];
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(protos, &["proto"])?;
    for p in protos {
        println!("cargo:rerun-if-changed={p}");
    }
    Ok(())
}
