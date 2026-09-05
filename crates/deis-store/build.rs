fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos: &[&str] = &["proto/storage.proto", "proto/blob.proto"];
    tonic_build::configure()
        // The client stubs are not used by the binary; they exist so the
        // crate's own tests can drive the real services over a real socket
        // instead of calling the handlers directly.
        .build_server(true)
        .build_client(true)
        .compile_protos_with_config(protobuf_config()?, protos, &["proto"])?;
    for p in protos {
        println!("cargo:rerun-if-changed={p}");
    }
    Ok(())
}

fn protobuf_config() -> Result<prost_build::Config, Box<dyn std::error::Error>> {
    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    Ok(config)
}
