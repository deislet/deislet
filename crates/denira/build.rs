fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The denix management proto is the single source of truth and lives in
    // the denix crate; only the client side is generated here.
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["../denix/proto/management.proto"], &["../denix/proto"])?;

    println!("cargo:rerun-if-changed=../denix/proto/management.proto");
    Ok(())
}
