fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The deis-runtime management proto is the single source of truth and lives in
    // the deis-runtime crate; only the client side is generated here.
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["../deis-runtime/proto/management.proto"], &["../deis-runtime/proto"])?;

    println!("cargo:rerun-if-changed=../deis-runtime/proto/management.proto");
    Ok(())
}
