fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos_with_config(protobuf_config()?, &["proto/migration.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/migration.proto");
    Ok(())
}

fn protobuf_config() -> Result<prost_build::Config, Box<dyn std::error::Error>> {
    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    Ok(config)
}
