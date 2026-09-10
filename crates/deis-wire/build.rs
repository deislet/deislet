fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos_with_config(
            protobuf_config()?,
            &["proto/control.proto", "proto/machine.proto"],
            &["proto"],
        )?;

    println!("cargo:rerun-if-changed=proto/control.proto");
    println!("cargo:rerun-if-changed=proto/machine.proto");
    Ok(())
}

fn protobuf_config() -> Result<prost_build::Config, Box<dyn std::error::Error>> {
    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    Ok(config)
}
